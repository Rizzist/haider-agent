//! [`HarnessActor`] — the single-writer run loop for one session.
//!
//! Owned invariants:
//! - **Single-writer envelope stamping.** Only this actor mints event ids and
//!   stamps `authority_epoch`/`worker_generation` for its session, and every
//!   envelope is appended through the [`StoreHandle`] before it is broadcast:
//!   subscribers only ever see committed facts.
//! - **Item lifecycle law.** Every streamed item is exactly
//!   started → delta* → completed. Text and reasoning items open lazily on
//!   their first delta and close before a tool call starts or the turn
//!   terminates; tool items close on `ToolCallEnd`, or with `Pending`/`Failed`/`Cancelled`
//!   status when a terminal path finds them still open.
//! - **Cancellation is an outcome, never an error.** A cancelled turn commits
//!   `RunState::Cancelled` as its final envelope and emits nothing after it,
//!   even when cancellation wins a race with a buffered provider event.
//! - **Retry owner (R6, authoritative site).** Provider retry lives here and
//!   ONLY here (adapters keep `RetryPolicy::Never`): up to `MAX_API_RETRIES`
//!   attempts per individual provider request, only retryable transport/
//!   rate-limit/overload errors, and never after that request emitted a stream
//!   event — which also fences effects, since a tool can only run after
//!   events. `wait_before_provider_retry` commits durable provider `Waiting`
//!   telemetry followed by `Retrying` (W-C M4: a visible `attempt K/max`
//!   counter, Claude-Code style) around the backoff, and cancellation wins
//!   every wait. The backoff is a PURE function
//!   of the attempt (`retry_backoff_ms`), and the wait is served through the
//!   injected [`RetrySleeper`] so laws assert the sequence without waiting.
//!
//! General tool calls run through the injected [`ToolDispatcher`] (W3c);
//! with no dispatcher installed they are surfaced completed-as-
//! `ToolStatus::Pending`, the pre-W3c standalone behavior. The actor owns the
//! two presentation tools because only it may journal the session's menu and
//! run-state envelopes: `request_input` keeps its blocking round trip, while
//! `plan` journals an immediate automatic acceptance without parking. Event
//! ids come from the [`EventIdGenerator`] namespace: supervisor-installed and
//! shared with the effect journal in the daemon, self-minted in standalone
//! use.

#[cfg(test)]
#[path = "peer_prompt_tests.rs"]
mod peer_prompt_tests;

use crate::{
    ArtifactReader, InteractionGate, InteractionResolution, InteractionResolutionPolicy,
    PromptHistoryCompiler, ProviderViewAppendRequest, StoreHandle, unix_time_ms,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, ChildReport, ChipState, ReportVerification};
use haider_protocol::cache::{
    CACHE_REQUEST_ATTEMPT_EXTENSION_KIND, CacheEpochTransitionReason, CacheEpochTransitionV1,
    CacheRequestAttemptV1, PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND, ProviderRequestAttemptV1,
    ProviderRequestKind, ProviderViewAttemptV1, ProviderViewBlobV1, ProviderViewLedgerV1,
};
use haider_protocol::context::{
    ContextAccounting, ContextCompactionTier, ContextEconomy, ContextFootprint,
    ContextFootprintTruth, ContextSavingsEvent, OutputSavings,
};
use haider_protocol::credential::RotationEvent;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CONTINUATION_CHECKPOINT_EXTENSION_KIND, CompactionIntent,
    CompactionResume, ContinuationCheckpoint, NodeKind, TodoState, TreeNode,
};
use haider_protocol::ids::{
    AgentId, ArtifactRef, BranchId, CredentialAlias, DeviceId, EventId, ItemId, MenuId, NodeId,
    RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    ErrorRecoveryCardKind, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::peer::PeerMessage;
use haider_protocol::provider::{
    AccountUsage, Block, CacheBreakpointHashesV1, CacheBreakpointV1, CacheControlObservationV1,
    CacheCostEstimate, CacheMissClassificationV1, CachePrefixMatchV1, CacheRequestDiagnosticV1,
    CacheRewarmReasonV1, CacheStatAvailability, FinishReason, NormalizedUsage,
    PROVIDER_OPAQUE_EXTENSION_KIND, PrefixDigests, PreviousCacheBreakpointV1, RequestUsage,
    StreamEvent, Usage, UsageRequestKind, UsageScope, WEB_SOURCES_EXTENSION_KIND, WebSource,
};
use haider_protocol::reply::{ReplyArenaWriter, ReplyText};
use haider_protocol::request_budget::{
    PROVIDER_REQUEST_BUDGET_EXTENSION_KIND, RequestBudgetContinuationV1, RequestBudgetPhaseV1,
    RequestBudgetStatusV1, RequestBudgetV1,
};
use haider_protocol::state::{RunState, WaitReason};
use haider_protocol::tool::{
    BoundedResult, ImageBlockRef, TOOL_RESULT_IMAGE_MAX_BYTES,
    TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN, TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN,
    TOOL_RESULT_IMAGE_MAX_DIMENSION, ToolResultStatus,
};
use haider_protocol::verify::VerifyVerdict;
use haider_provider::{
    Message, PROVIDER_DEADLINE_SAFETY_MARGIN, PromptCacheMetadata, Provider, ProviderError,
    ProviderErrorKind, ProviderRequestOrdinal, ProviderStream, ProviderStreamItem,
    ProviderTimeoutReason, ROUTE_STATE_POLL_INTERVAL, ResolvedAttachment, ToolDefinition,
    TurnRequest, TurnTraceContext, apply_tool_result_image_budget,
    before_provider_request_deadline, canonical_tool_definitions,
    canonical_tool_definitions_digest, deadline_exhausted_error,
    degrade_tool_result_images_to_placeholders, effective_request_budget,
    validate_provider_view_prefix,
};

pub const ROUTE_REPLAY_ATTEMPT_EXTENSION_KIND: &str = "haider.route_replay_attempt.v1";
pub const ROUTE_REPLAY_EVENT_EXTENSION_KIND: &str = "haider.route_replay_event.v1";

/// Frames one boundary-delivered peer message for the provider tail.
///
/// The whole frame is deliberately a new user-role message because provider
/// APIs do not have a peer role, but the protocol-owned rendering applies the
/// exact trust label and makes clear that it is neither a user nor system
/// instruction. Callers append this message at the next turn boundary; they
/// must never splice it into an earlier history entry.
#[must_use]
pub fn peer_message_for_provider(message: &PeerMessage) -> Message {
    Message::user_text(message.render_for_prompt())
}

/// Appends a peer frame after the already-compiled conversation. This is the
/// only supported insertion operation: prior messages—and therefore the
/// provider's reusable cached prefix—are left byte-for-byte untouched.
pub fn append_peer_message_to_provider_tail(messages: &mut Vec<Message>, message: &PeerMessage) {
    messages.push(peer_message_for_provider(message));
}

/// Shapes a tool result only at the provider boundary.
///
/// The durable [`BoundedResult`] remains the authority and is never rewritten.
/// Peer/SSH inventories and remote command results can be large, so their
/// first-send and replay views use the same deterministic byte cap while the
/// journal retains the raw JSON.
struct ModelToolResultProjection {
    preview: String,
    truncated: bool,
    savings: Option<OutputSavings>,
}

fn model_tool_result_projection(
    tool_name: &str,
    result: &BoundedResult,
) -> ModelToolResultProjection {
    const INVENTORY_MODEL_PREVIEW_MAX_BYTES: usize = 8 * 1024;
    let disclosed_omission = inline_text_elision_disclosure(&result.preview);
    if tool_name == "process_exec"
        && result.truncated
        && let Some(savings) = trusted_process_output_savings(&result.preview)
    {
        return ModelToolResultProjection {
            preview: result.preview.clone(),
            truncated: true,
            savings: Some(savings),
        };
    }
    let scope = match tool_name {
        "peer_list" => "peer_list_model_boundary",
        "ssh_list" => "ssh_inventory_model_boundary",
        "ssh_shell" => "remote_shell_model_boundary",
        // Keep the existing local process adapter byte-for-byte unchanged.
        // Only the SSH-profile form opts into this additional model-only cap.
        "process_exec"
            if matches!(
                serde_json::from_str::<serde_json::Value>(&result.preview)
                    .ok()
                    .and_then(|value| value.get("remote").and_then(serde_json::Value::as_bool)),
                Some(true)
            ) =>
        {
            "remote_process_model_boundary"
        }
        _ => {
            if result.truncated {
                let (omitted_bytes_at_least, omitted_bytes_exact) =
                    disclosed_omission.unwrap_or((1, false));
                let elided = haider_tools::mark_text_elision(
                    &result.preview,
                    INVENTORY_MODEL_PREVIEW_MAX_BYTES,
                    "bounded_tool_result",
                    omitted_bytes_at_least,
                    omitted_bytes_exact,
                );
                return ModelToolResultProjection {
                    preview: elided.text,
                    truncated: true,
                    savings: Some(elided.savings),
                };
            }
            return ModelToolResultProjection {
                preview: result.preview.clone(),
                truncated: result.truncated,
                savings: None,
            };
        }
    };
    if result.preview.len() <= INVENTORY_MODEL_PREVIEW_MAX_BYTES {
        if result.truncated {
            let (omitted_bytes_at_least, omitted_bytes_exact) =
                disclosed_omission.unwrap_or((1, false));
            let elided = haider_tools::mark_text_elision(
                &result.preview,
                INVENTORY_MODEL_PREVIEW_MAX_BYTES,
                scope,
                omitted_bytes_at_least,
                omitted_bytes_exact,
            );
            return ModelToolResultProjection {
                preview: elided.text,
                truncated: true,
                savings: Some(elided.savings),
            };
        }
        return ModelToolResultProjection {
            preview: result.preview.clone(),
            truncated: result.truncated,
            savings: None,
        };
    }
    let elided = disclosed_omission.map_or_else(
        || {
            haider_tools::elide_text_head_tail(
                &result.preview,
                INVENTORY_MODEL_PREVIEW_MAX_BYTES,
                scope,
            )
            .unwrap_or_else(|| haider_tools::mark_text_elision(&result.preview, 0, scope, 1, false))
        },
        |(omitted_bytes_at_least, omitted_bytes_exact)| {
            haider_tools::mark_text_elision(
                &result.preview,
                INVENTORY_MODEL_PREVIEW_MAX_BYTES,
                scope,
                omitted_bytes_at_least,
                omitted_bytes_exact,
            )
        },
    );
    ModelToolResultProjection {
        preview: elided.text,
        truncated: true,
        savings: Some(elided.savings),
    }
}

/// Reads disclosure-only inline markers so the actor can attach their source
/// omission to the one authoritative output event. The maximum is used rather
/// than a sum because nested markers can restate an upstream omission. Image
/// byte sizes are not text-token estimates and are deliberately excluded.
fn inline_text_elision_disclosure(preview: &str) -> Option<(usize, bool)> {
    let mut omitted_bytes_at_least = None::<u64>;
    let mut omitted_bytes_exact = true;
    for line in preview.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(marker) = value.get("haider_elision_v1") else {
            continue;
        };
        if marker.get("omitted_images").is_some() {
            continue;
        }
        let Some(omitted) = marker
            .get("omitted_bytes")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        omitted_bytes_at_least =
            Some(omitted_bytes_at_least.map_or(omitted, |seen| seen.max(omitted)));
        omitted_bytes_exact &= marker
            .get("omitted_bytes_exact")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    }
    omitted_bytes_at_least.map(|omitted| {
        (
            usize::try_from(omitted.max(1)).unwrap_or(usize::MAX),
            omitted_bytes_exact,
        )
    })
}

/// Accepts only the daemon's typed process-result envelope. Shell text that
/// merely contains an economy key cannot suppress the generic boundary marker.
fn trusted_process_output_savings(preview: &str) -> Option<OutputSavings> {
    let value = serde_json::from_str::<serde_json::Value>(preview).ok()?;
    for required in [
        "status",
        "effect_id",
        "command_arg_digest",
        "transcript_digest",
        "output_adapter",
    ] {
        value.get(required)?;
    }
    let output = value.get("output")?.as_str()?;
    let has_machine_marker = output.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| value.get("haider_elision_v1").cloned())
            .is_some()
    });
    if !has_machine_marker {
        return None;
    }
    let savings =
        serde_json::from_value::<OutputSavings>(value.get("context_savings_detail")?.clone())
            .ok()?;
    (savings.scope == "process_result_model_boundary"
        && savings.output_bytes
            == u64::try_from(haider_tools::provider_request_text_projection_bytes(
                preview,
            ))
            .unwrap_or(u64::MAX))
    .then_some(savings)
}

pub(crate) fn model_tool_result_preview(tool_name: &str, result: &BoundedResult) -> (String, bool) {
    let projection = model_tool_result_projection(tool_name, result);
    (projection.preview, projection.truncated)
}
use haider_tools::{Plan, RequestInput, TodoWrite};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};

#[cfg(test)]
#[path = "actor_request_attempt_tests.rs"]
mod actor_request_attempt_tests;

#[cfg(test)]
#[path = "actor_tool_result_tests.rs"]
mod actor_tool_result_tests;

#[cfg(test)]
#[path = "actor_context_economy_tests.rs"]
mod actor_context_economy_tests;

// Two soft tranches cover the reported 53-round solved benchmark with 11
// requests of headroom, while preserving a finite guard against runaway work.
const DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN: usize = 64;

/// Profile-scoped secret used only for diagnostic prefix fingerprints.
/// Custom debug output is deliberately redacted so a config dump cannot
/// disclose the key that protects short prompt components from guessing.
#[derive(Clone)]
pub struct CacheDiagnosticKey([u8; 32]);

impl CacheDiagnosticKey {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn ephemeral(session_id: &SessionId, device_id: &DeviceId) -> Self {
        use std::io::Read as _;

        let mut bytes = [0_u8; 32];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .is_err()
        {
            // Production daemon journals replace this process-local key with
            // the profile key created from `getrandom`. This fallback keeps
            // direct core embedders functional on platforms without
            // `/dev/urandom`, while mixing RandomState's per-process secret.
            use std::hash::{BuildHasher as _, Hasher as _};

            for (index, chunk) in bytes.chunks_exact_mut(8).enumerate() {
                let random = std::collections::hash_map::RandomState::new();
                let mut entropy = random.build_hasher();
                entropy.write(session_id.as_str().as_bytes());
                entropy.write(device_id.as_str().as_bytes());
                entropy.write_u32(std::process::id());
                entropy.write_u64(unix_time_ms());
                entropy.write_usize(index);
                chunk.copy_from_slice(&entropy.finish().to_le_bytes());
            }
        }
        Self(bytes)
    }
}

impl std::fmt::Debug for CacheDiagnosticKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CacheDiagnosticKey([REDACTED])")
    }
}

/// The immediately preceding request boundary loaded by the daemon or
/// retained between tool-loop requests in the same actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousCacheRequest {
    pub history_message_count: usize,
    pub breakpoint_hashes: CacheBreakpointHashesV1,
    pub cache_domain_hash: Option<String>,
}
const DEFAULT_MAX_CONTINUATIONS_PER_TURN: usize = 8;
/// Maximum time a provider-stream text, reasoning, or tool-argument delta may
/// remain in memory before it is journaled. Contiguous deltas for one item and
/// one variant coalesce during this window; every semantic boundary flushes
/// earlier, so `Completed` remains the authoritative durable item.
pub const STREAM_DELTA_COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);
/// Bounded web-sources list journaled under one finished turn (W-B).
const WEB_SOURCES_CAP: usize = 8;
const DEFAULT_DEFERRED_COMMAND_CAPACITY: usize = 64;
/// W-C M4: Claude-Code-style visible API-error retry ceiling. Attempt 1 is
/// the original try; up to nine more re-issues follow before the failure
/// latches `Errored`, and the status line counts `attempt K/MAX_API_RETRIES`.
const MAX_API_RETRIES: usize = 10;
/// Exponential backoff base before the run-scoped deterministic jitter is
/// applied. One second reads cleanly as `Retrying in 1s`.
const RETRY_BASE_MS: u64 = 1_000;
/// Backoff cap: a single wait never exceeds ~30s of computed delay.
const RETRY_CEILING_MS: u64 = 30_000;
/// Provider instructions are respected beyond the computed-jitter ceiling,
/// but a daemon must not park one request indefinitely on an untrusted value.
/// Values above one minute terminalize as retryable exhaustion.
const MAX_PROVIDER_RETRY_AFTER_MS: u64 = 60_000;
/// Minimum percentage of the pre-compaction provider input footprint that a
/// compaction must free. Below this bound, another compaction in the same turn
/// is more likely to thrash than recover useful context capacity.
pub const COMPACTION_MIN_FREED_PERCENT: u64 = 15;
/// Fast-mode structural tier one. Starting above 50% avoids invalidating a
/// still-useful provider cache for conversations that have ample headroom.
pub const CONTEXT_STRUCTURAL_TIER_ONE_PERCENT: u64 = 60;
/// Fast-mode structural tier two. Ten percentage points remain before the
/// existing model-summary boundary, leaving the cheaper trim one full chance
/// to recover headroom first.
pub const CONTEXT_STRUCTURAL_TIER_TWO_PERCENT: u64 = 75;
/// Existing provider-summary boundary retained for compatibility.
pub const CONTEXT_SUMMARY_TIER_PERCENT: u64 = 85;
pub const CONTEXT_STRUCTURAL_TIER_ONE_RETAINED_TOOL_PAIRS: usize = 24;
pub const CONTEXT_STRUCTURAL_TIER_TWO_RETAINED_TOOL_PAIRS: usize = 12;

static TURN_TRACE_REGISTRY: OnceLock<Mutex<HashMap<(SessionId, RunId), TurnTraceContext>>> =
    OnceLock::new();

/// Legacy numeric correlation retained for the client-side terminal trace.
/// Provider and daemon request-attempt records use the declared session/run/
/// turn/request coordinates instead; this digest is not a transport identity.
#[doc(hidden)]
#[must_use]
pub fn turn_trace_ordinal(session_id: &SessionId, accepted_seq: u64) -> u64 {
    let mut digest = 0xcbf2_9ce4_8422_2325_u64;
    for byte in session_id
        .as_str()
        .as_bytes()
        .iter()
        .copied()
        .chain(accepted_seq.to_le_bytes())
    {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest.max(1)
}

/// Registers one trace context after durable turn acceptance. The composite
/// key prevents equal run IDs in distinct sessions from aliasing each other.
#[doc(hidden)]
pub fn register_turn_trace(session_id: SessionId, run_id: RunId, trace: TurnTraceContext) {
    let mut registry = TURN_TRACE_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    registry.insert((session_id, run_id), trace);
}

/// Returns the accepted trace context for worker/provider setup.
#[doc(hidden)]
#[must_use]
pub fn registered_turn_trace(session_id: &SessionId, run_id: &RunId) -> Option<TurnTraceContext> {
    TURN_TRACE_REGISTRY.get().and_then(|registry| {
        registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(session_id.clone(), run_id.clone()))
            .cloned()
    })
}

/// Resolves a committed run batch without exposing its durable identity to
/// telemetry. One journal append never spans logical runs.
#[doc(hidden)]
#[must_use]
pub fn turn_trace_for_envelopes(envelopes: &[RawEnvelope]) -> Option<TurnTraceContext> {
    let envelope = envelopes
        .iter()
        .find(|envelope| envelope.run_id.is_some())?;
    registered_turn_trace(&envelope.session_id, envelope.run_id.as_ref()?)
}

#[doc(hidden)]
#[must_use]
pub fn envelopes_contain_terminal(envelopes: &[RawEnvelope]) -> bool {
    envelopes.iter().any(raw_envelope_is_terminal)
}

/// Releases the registry's bounded lookup after a terminal batch has been
/// committed and published. Live actor/provider owners may retain their Arc
/// briefly, but no future turn can resolve this entry.
#[doc(hidden)]
pub fn unregister_turn_trace_for_envelopes(envelopes: &[RawEnvelope]) {
    let Some((session_id, run_id)) = envelopes.iter().find_map(|envelope| {
        envelope
            .run_id
            .as_ref()
            .map(|run_id| (&envelope.session_id, run_id))
    }) else {
        return;
    };
    if let Some(registry) = TURN_TRACE_REGISTRY.get() {
        registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(session_id.clone(), run_id.clone()));
    }
}

/// Immutable identity and fencing parameters for one session actor.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub session_id: SessionId,
    pub branch_id: Option<BranchId>,
    pub agent_id: Option<AgentId>,
    pub device_id: DeviceId,
    pub authority_epoch: u64,
    pub worker_generation: u64,
    pub model: String,
    pub max_tokens: u64,
    /// Absolute deadline for provider-open work in this logical turn. The
    /// daemon derives it from headless/client budgets; interactive sessions
    /// leave it absent and retain provider-local timeout defaults.
    pub provider_deadline: Option<tokio::time::Instant>,
    /// Provider-declared active-model context window. `None` stays unknown;
    /// inferred adapter tables are not authoritative for compaction policy.
    pub context_window: Option<u64>,
    /// Daemon-validated space reserved for provider output on every request.
    pub reserved_output_tokens: u64,
    /// Whether provider-reported cached input is already included in
    /// `Usage.input`. OpenAI-style adapters report a subset; Anthropic-style
    /// adapters report cache reads separately. The daemon pins this with the
    /// provider so footprint splits never double-count cache hits.
    pub cached_input_is_subset: bool,
    /// Enables W7b proactive thresholding and durable footprint snapshots.
    /// Daemons set this when serving `context_compaction_v1`; standalone
    /// embeddings retain W7a hard-fit behavior unless they opt in.
    pub context_compaction_v1: bool,
    /// Enables the two model-free whole-tool-pair tiers. Daemon sessions bind
    /// this to durable fast mode; default mode remains summarize-only.
    pub structural_context_trimming: bool,
    /// Restart-stable estimated savings loaded from typed session metadata.
    pub context_economy: ContextEconomy,
    /// Enables the post-compaction runaway guard and promotion path. This is
    /// independent from proactive compaction for additive feature rollout.
    pub compaction_guard_v1: bool,
    /// Deterministic daemon-owned policy bound to every request in this actor.
    pub system_prompt: Option<String>,
    /// Ephemeral daemon-authored user-role context snapshotted once per
    /// logical provider request
    /// and inserted immediately before the accepted current-user message. It
    /// is never journaled or hashed into the durable stable prefix. The
    /// snapshot is intentionally byte-stable through physical transport
    /// retries of that request; Convergence Graph M1 uses this for
    /// `GraphBrief` and typed workflows rebind it after each completed stage.
    pub volatile_user_tail: Option<String>,
    /// General tools the paired dispatcher can execute.
    pub tools: Vec<ToolDefinition>,
    /// Daemon-owned immutable pack; standalone embedders keep using `tools`.
    shared_tools: Option<Arc<[ToolDefinition]>>,
    /// Canonical digest of `tools` when the daemon installed an immutable
    /// tool-pack view. Standalone embedders that mutate `tools` directly leave
    /// this unset and retain canonical on-demand behavior.
    tool_pack_digest: Option<String>,
    /// Enforce `tools` as an authorization ceiling even for root sessions.
    /// Delegated children always enforce it through `agent_id`; daemon-owned
    /// workflows enable this bit so actor-owned tools cannot bypass a dynamic
    /// dispatcher grant merely by naming an unadvertised tool.
    pub enforce_advertised_tool_ceiling: bool,
    /// Local equivalents advertised after one exact provider-hosted-tool
    /// rejection. Empty means this provider has no safe fallback pack.
    pub provider_tool_fallback_tools: Vec<ToolDefinition>,
    shared_provider_tool_fallback_tools: Option<Arc<[ToolDefinition]>>,
    /// Canonical digest paired with `provider_tool_fallback_tools`.
    provider_tool_fallback_digest: Option<String>,
    /// Frozen turn-authorized tool pack before provider-specific local web
    /// selection. `None` leaves standalone embedders' packs untouched.
    pub provider_tool_base: Option<Vec<ToolDefinition>>,
    shared_provider_tool_base: Option<Arc<[ToolDefinition]>>,
    shared_provider_tool_variants: SharedToolPackVariants,
    /// Turn-authorized local web definitions within [`Self::provider_tool_base`].
    /// Non-web user and registry tools never enter this pool.
    pub provider_local_web_tools: Vec<ToolDefinition>,
    shared_provider_local_web_tool_names: Arc<[String]>,
    /// CAS-backed attachments resolved before crossing the provider boundary.
    pub attachments: Vec<ResolvedAttachment>,
    /// Whether the resolved provider/model accepts vision inputs. Unsupported
    /// providers receive artifact-naming placeholders for tool images.
    pub tool_result_images_supported: bool,
    /// Account pinned by the turn-scoped provider resolver.
    pub usage_account: Option<CredentialAlias>,
    /// Non-secret provider/model/auth/cache-domain coordinates attached to
    /// usage telemetry after the provider response is decoded.
    pub usage_scope: UsageScope,
    /// Compiler-provided exclusive end of immutable history. `None` keeps
    /// standalone callers on the structural current-user boundary.
    pub cache_stable_history_end: Option<usize>,
    /// Compiler-provided current-user start for adapter cache metadata.
    pub cache_current_user_start: Option<usize>,
    /// Active C3 fork-cohort root. `None` isolates routing to `session_id`;
    /// only a byte-identical inherited provider-view segment sets this, and
    /// provider-view divergence clears it on the next turn setup.
    pub cache_cohort: Option<String>,
    /// Exclusive end of the latest active compaction-summary message.
    pub cache_compaction_summary_end: Option<usize>,
    /// Conservative future-read expectation for explicit cache resources.
    pub cache_expected_later_reads: u32,
    /// Observed gap in the current ephemeral cache domain.
    pub cache_reuse_gap_ms: Option<u64>,
    /// Secret profile key for non-reversible cache diagnostics.
    pub cache_diagnostic_key: CacheDiagnosticKey,
    /// Last completed provider request in this cache lane, when durable
    /// request-level telemetry exists.
    pub cache_previous_request: Option<PreviousCacheRequest>,
    /// Exact immutable provider view from the preceding durable send. Unlike
    /// request diagnostics, this survives restart so same-epoch middle
    /// mutations can stop before provider traffic.
    pub cache_previous_provider_view: Option<ProviderViewLedgerV1>,
    /// Deliberate cold-boundary marker consumed by the first request that
    /// actually yields usage telemetry.
    pub cache_initial_rewarm: Option<CacheRewarmReasonV1>,
    /// Canonical provider-visible reasoning/fast settings used only for the
    /// prefix digest. It never enters a request body from this field.
    pub reasoning_settings: String,
    /// A factory-time alternate that must be committed before provider work.
    pub initial_rotation: Option<RotationEvent>,
    /// Logical-turn-wide one-hop allowance. This is distinct from provider
    /// attempt counters, which reset between tool-loop requests.
    pub rotation_budget_consumed: bool,
    /// Daemon-owned resolver consulted only at a pre-first-event boundary.
    pub provider_attempt_resolver: Option<Arc<dyn ProviderAttemptResolver>>,
    /// Daemon-owned durable pair-selection seam. Automatic fallback and
    /// promotion refuse to switch unless this is installed.
    pub provider_pair_switch_committer: Option<Arc<dyn ProviderPairSwitchCommitter>>,
    /// Optional already-resolved larger-context lane used by the compaction
    /// runaway guard. Daemons prove its credential and window ordering.
    pub compaction_promotion: Option<ProviderPairSwitchTarget>,
    /// Injected backoff wait for the M4 provider-retry seam. Defaults to a
    /// real `tokio` timer; laws swap in an instant recording sleeper.
    pub retry_sleeper: Arc<dyn RetrySleeper>,
    /// Durable compaction implementation installed by the daemon. Standalone
    /// actors surface context overflow when none is configured.
    pub context_compactor: Option<Arc<dyn ContextCompactor>>,
    /// Daemon-owned provider EndTurn guard. Standalone actors have no graph
    /// authority and therefore leave this unset.
    pub finalization_guard: Option<Arc<dyn FinalizationGuard>>,
    /// Maps a provider-open deadline owned by the daemon's run budget back to
    /// that typed budget terminal. A distinct client request deadline remains
    /// a provider timeout, and standalone actors leave this unset.
    pub provider_deadline_guard: Option<Arc<dyn ProviderDeadlineGuard>>,
    /// Daemon-owned hard-budget authority at the physical provider-request
    /// boundary. Standalone actors leave this unset.
    pub provider_budget_guard: Option<Arc<dyn ProviderBudgetGuard>>,
    /// Typed human-availability policy derived from durable session metadata.
    pub interaction_policy: InteractionResolutionPolicy,
    pub command_capacity: usize,
    pub broadcast_capacity: usize,
    /// Hard ceiling on provider requests made by one logical turn.
    pub max_provider_requests_per_turn: usize,
    /// Warn the model once after this many logical requests. Transport retries
    /// do not spend another request. A fresh continuation turn resets the budget.
    pub provider_request_tranche: usize,
    /// Logical requests already spent before a restart-safe continuation was
    /// reconstructed. Ordinary accepted turns start at zero.
    pub provider_requests_already_made: usize,
    /// Physical request-attempt ordinal already consumed before recovery.
    /// This keeps durable cache/provider-view coordinates monotonic.
    pub provider_request_ordinal_already_made: u64,
    /// Durable 1-based run/turn coordinate allocated by the session store.
    pub turn_ordinal: u64,
    /// Shared physical-attempt namespace for actor, compaction, and auxiliary
    /// turn-owned provider work. Standalone embedders may leave it absent.
    pub provider_request_ordinals: Option<ProviderRequestOrdinal>,
    /// Daemon-owned journal hook for auxiliary HTTP calls issued inside an
    /// adapter under the current primary request (for example cache CRUD).
    pub provider_request_attempt_recorder:
        Option<Arc<dyn haider_provider::ProviderRequestAttemptRecorder>>,
    /// A restart-reconstructed admission retry has no trustworthy in-memory
    /// cumulative baseline. Keep each durable response snapshot request-local
    /// while retaining cumulative accounting inside the actor.
    pub recovery_request_local_usage: bool,
    /// Independent guard against providers repeatedly exhausting output.
    pub max_continuations_per_turn: usize,
    /// Maximum number of submissions parked behind the active turn.
    pub deferred_command_capacity: usize,
    /// Daemon supervisors close/reconcile their effect broker before writing
    /// `Cancelled`. Standalone actors retain the direct terminal commit.
    pub supervisor_commits_cancelled: bool,
    /// Maximum time a provider-stream delta may remain non-durable. Set this
    /// to `Duration::ZERO` to disable coalescing and restore one durable
    /// envelope per provider delta for a deployment that needs that cadence.
    pub stream_delta_coalesce_window: std::time::Duration,
    /// Content-free, opt-in per-turn timing correlation. `None` is the normal
    /// trace-off path and performs no counters or clock reads.
    pub turn_trace: Option<TurnTraceContext>,
    /// Optional supervisor-owned event namespace shared by every turn actor
    /// and effect journal in one worker generation.
    event_ids: Option<Arc<EventIdGenerator>>,
    started_at_ms: Option<u64>,
}

/// Shared lookup table for provider-selected variants of one immutable pack.
pub type SharedToolPackVariants = Arc<HashMap<Vec<String>, (Arc<[ToolDefinition]>, String)>>;

/// Immutable daemon-cached tool packs installed together for one provider lane.
#[derive(Debug, Clone)]
pub struct SharedToolPacks {
    pub base: Arc<[ToolDefinition]>,
    pub local_web_tool_names: Arc<[String]>,
    pub current: Arc<[ToolDefinition]>,
    pub current_digest: String,
    pub fallback: Option<(Arc<[ToolDefinition]>, String)>,
    pub variants: SharedToolPackVariants,
}

impl HarnessConfig {
    /// Convenience constructor with v0 defaults (fake model, small channels).
    pub fn for_session(
        session_id: SessionId,
        device_id: DeviceId,
        authority_epoch: u64,
        worker_generation: u64,
    ) -> Self {
        let cache_diagnostic_key = CacheDiagnosticKey::ephemeral(&session_id, &device_id);
        Self {
            session_id,
            branch_id: None,
            agent_id: None,
            device_id,
            authority_epoch,
            worker_generation,
            model: "fake-model".into(),
            max_tokens: 4096,
            provider_deadline: None,
            context_window: None,
            reserved_output_tokens: 4096,
            cached_input_is_subset: true,
            context_compaction_v1: false,
            structural_context_trimming: false,
            context_economy: ContextEconomy::default(),
            compaction_guard_v1: false,
            system_prompt: None,
            volatile_user_tail: None,
            tools: Vec::new(),
            shared_tools: None,
            tool_pack_digest: None,
            enforce_advertised_tool_ceiling: false,
            provider_tool_fallback_tools: Vec::new(),
            shared_provider_tool_fallback_tools: None,
            provider_tool_fallback_digest: None,
            provider_tool_base: None,
            shared_provider_tool_base: None,
            shared_provider_tool_variants: Arc::default(),
            provider_local_web_tools: Vec::new(),
            shared_provider_local_web_tool_names: Arc::default(),
            attachments: Vec::new(),
            tool_result_images_supported: false,
            usage_account: None,
            usage_scope: UsageScope::default(),
            cache_stable_history_end: None,
            cache_current_user_start: None,
            cache_cohort: None,
            cache_compaction_summary_end: None,
            cache_expected_later_reads: 0,
            cache_reuse_gap_ms: None,
            cache_diagnostic_key,
            cache_previous_request: None,
            cache_previous_provider_view: None,
            cache_initial_rewarm: None,
            reasoning_settings: String::new(),
            initial_rotation: None,
            rotation_budget_consumed: false,
            provider_attempt_resolver: None,
            provider_pair_switch_committer: None,
            compaction_promotion: None,
            retry_sleeper: Arc::new(RealRetrySleeper),
            context_compactor: None,
            finalization_guard: None,
            provider_deadline_guard: None,
            provider_budget_guard: None,
            interaction_policy: InteractionResolutionPolicy::default(),
            command_capacity: 8,
            broadcast_capacity: 128,
            max_provider_requests_per_turn: DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN,
            provider_request_tranche: 32,
            provider_requests_already_made: 0,
            provider_request_ordinal_already_made: 0,
            turn_ordinal: 1,
            provider_request_ordinals: None,
            provider_request_attempt_recorder: None,
            recovery_request_local_usage: false,
            max_continuations_per_turn: DEFAULT_MAX_CONTINUATIONS_PER_TURN,
            deferred_command_capacity: DEFAULT_DEFERRED_COMMAND_CAPACITY,
            supervisor_commits_cancelled: false,
            stream_delta_coalesce_window: STREAM_DELTA_COALESCE_WINDOW,
            turn_trace: None,
            event_ids: None,
            started_at_ms: None,
        }
    }

    /// Installs the worker-generation namespace that owns all event IDs.
    #[must_use]
    pub fn with_event_ids(mut self, event_ids: Arc<EventIdGenerator>) -> Self {
        self.event_ids = Some(event_ids);
        self
    }

    /// Overrides the wall-clock component of minted IDs.
    ///
    /// This is an injection seam for deterministic restart tests. Durable
    /// `worker_generation`, rather than clock uniqueness, must prevent ID
    /// collisions when two actors receive the same value here.
    pub fn with_started_at_ms(mut self, started_at_ms: u64) -> Self {
        self.started_at_ms = Some(started_at_ms);
        self
    }

    /// Overrides the provider-delta coalescing cadence for this actor.
    /// `Duration::ZERO` restores the historical envelope-per-delta cadence.
    #[must_use]
    pub fn with_stream_delta_coalesce_window(mut self, window: std::time::Duration) -> Self {
        self.stream_delta_coalesce_window = window;
        self
    }

    /// Replaces only provider-selected local web definitions, preserving the
    /// rest of the turn-authorized tool pack byte-for-byte.
    pub fn install_provider_derived_request_state(&mut self, state: &ProviderDerivedRequestState) {
        let current_key = normalized_selected_tool_names(
            &state.local_web_tool_names,
            &self.shared_provider_local_web_tool_names,
        );
        if let Some((tools, digest)) = self.shared_provider_tool_variants.get(&current_key) {
            self.tools.clear();
            self.shared_tools = Some(Arc::clone(tools));
            self.tool_pack_digest = Some(digest.clone());
            self.provider_tool_fallback_tools.clear();
            let fallback_key = normalized_selected_tool_names(
                &state.provider_fallback_local_web_tool_names,
                &self.shared_provider_local_web_tool_names,
            );
            let fallback = if state.provider_fallback_local_web_tool_names.is_empty() {
                None
            } else {
                self.shared_provider_tool_variants.get(&fallback_key)
            };
            self.shared_provider_tool_fallback_tools = fallback.map(|(tools, _)| Arc::clone(tools));
            self.provider_tool_fallback_digest = fallback.map(|(_, digest)| digest.clone());
            self.tool_result_images_supported = state.tool_result_images_supported;
            return;
        }
        if let Some(base) = self.shared_provider_tool_base.as_ref() {
            let tools: Arc<[ToolDefinition]> = select_provider_tools_by_name(
                base,
                &self.shared_provider_local_web_tool_names,
                &state.local_web_tool_names,
            )
            .into();
            self.tool_pack_digest = Some(canonical_tool_definitions_digest(&tools));
            self.tools.clear();
            self.shared_tools = Some(tools);
            self.provider_tool_fallback_tools.clear();
            self.shared_provider_tool_fallback_tools =
                if state.provider_fallback_local_web_tool_names.is_empty() {
                    None
                } else {
                    Some(
                        select_provider_tools_by_name(
                            base,
                            &self.shared_provider_local_web_tool_names,
                            &state.provider_fallback_local_web_tool_names,
                        )
                        .into(),
                    )
                };
            self.provider_tool_fallback_digest = self
                .shared_provider_tool_fallback_tools
                .as_deref()
                .map(canonical_tool_definitions_digest);
            self.tool_result_images_supported = state.tool_result_images_supported;
            return;
        }
        if let Some(base) = self.provider_tool_base.as_ref() {
            self.tools = canonical_tool_definitions(&select_provider_tools(
                base,
                &self.provider_local_web_tools,
                &state.local_web_tool_names,
            ));
            self.shared_tools = None;
            // These vectors are a pre-existing public standalone seam and can
            // be mutated after installation. Only immutable Arc-backed packs
            // may retain a cached digest.
            self.tool_pack_digest = None;
            self.provider_tool_fallback_tools =
                if state.provider_fallback_local_web_tool_names.is_empty() {
                    Vec::new()
                } else {
                    canonical_tool_definitions(&select_provider_tools(
                        base,
                        &self.provider_local_web_tools,
                        &state.provider_fallback_local_web_tool_names,
                    ))
                };
            self.shared_provider_tool_fallback_tools = None;
            self.provider_tool_fallback_digest = None;
            self.tool_result_images_supported = state.tool_result_images_supported;
            return;
        }
        self.shared_tools = None;
        self.tool_pack_digest = None;
        self.provider_tool_fallback_tools.clear();
        self.shared_provider_tool_fallback_tools = None;
        self.provider_tool_fallback_digest = None;
        self.tool_result_images_supported = state.tool_result_images_supported;
    }

    /// Installs daemon-cached immutable packs for the initial provider lane.
    /// Pair switches can still derive a new selection from the shared base.
    pub fn install_shared_tool_packs(
        &mut self,
        packs: SharedToolPacks,
        state: &ProviderDerivedRequestState,
    ) {
        let SharedToolPacks {
            base,
            local_web_tool_names,
            current,
            current_digest,
            fallback,
            variants,
        } = packs;
        self.provider_local_web_tools.clear();
        self.shared_provider_local_web_tool_names = local_web_tool_names;
        self.provider_tool_base = None;
        self.shared_provider_tool_base = Some(base);
        self.shared_provider_tool_variants = variants;
        self.tools.clear();
        self.shared_tools = Some(current);
        self.tool_pack_digest = Some(current_digest);
        self.provider_tool_fallback_tools.clear();
        let (fallback_tools, fallback_digest) = fallback.unzip();
        self.shared_provider_tool_fallback_tools = fallback_tools;
        self.provider_tool_fallback_digest = fallback_digest;
        self.tool_result_images_supported = state.tool_result_images_supported;
    }

    /// Returns the active definitions without materializing an immutable pack.
    #[must_use]
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        self.shared_tools.as_deref().unwrap_or(&self.tools)
    }

    /// Clones only the active Arc handle. Standalone definitions are promoted
    /// to shared storage once at this boundary.
    #[must_use]
    pub fn shared_tool_definitions(&self) -> Arc<[ToolDefinition]> {
        self.shared_tools
            .as_ref()
            .map_or_else(|| self.tools.clone().into(), Arc::clone)
    }

    fn has_provider_tool_fallback(&self) -> bool {
        self.shared_provider_tool_fallback_tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
            || !self.provider_tool_fallback_tools.is_empty()
    }

    fn activate_provider_tool_fallback(&mut self) {
        self.shared_tools = self.shared_provider_tool_fallback_tools.take();
        self.tools = if self.shared_tools.is_some() {
            Vec::new()
        } else {
            std::mem::take(&mut self.provider_tool_fallback_tools)
        };
        self.tool_pack_digest = self.provider_tool_fallback_digest.take();
    }

    /// Returns the pinned tool-pack digest, or canonicalizes directly for a
    /// standalone config whose public `tools` field was populated manually.
    #[must_use]
    pub fn canonical_tool_pack_digest(&self) -> String {
        self.tool_pack_digest
            .clone()
            .unwrap_or_else(|| canonical_tool_definitions_digest(self.tool_definitions()))
    }
}

fn select_provider_tools(
    base: &[ToolDefinition],
    provider_local_web_tools: &[ToolDefinition],
    selected_names: &[String],
) -> Vec<ToolDefinition> {
    base.iter()
        .filter(|definition| {
            !provider_local_web_tools
                .iter()
                .any(|candidate| candidate == *definition)
                || selected_names.iter().any(|name| name == &definition.name)
        })
        .cloned()
        .collect()
}

fn select_provider_tools_by_name(
    base: &[ToolDefinition],
    provider_local_web_tool_names: &[String],
    selected_names: &[String],
) -> Vec<ToolDefinition> {
    base.iter()
        .filter(|definition| {
            !provider_local_web_tool_names
                .iter()
                .any(|candidate| candidate == &definition.name)
                || selected_names.iter().any(|name| name == &definition.name)
        })
        .cloned()
        .collect()
}

fn normalized_selected_tool_names(names: &[String], available: &[String]) -> Vec<String> {
    let mut names = names
        .iter()
        .filter(|name| available.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    names
}

/// Thread-safe event-ID namespace shared by core and tool journals.
#[derive(Debug)]
pub struct EventIdGenerator {
    namespace: String,
    next: AtomicU64,
}

impl EventIdGenerator {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            next: AtomicU64::new(0),
        }
    }

    pub fn next(&self) -> EventId {
        let number = self.next.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        EventId::new(format!("{}-{number}", self.namespace))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitTurn {
    pub text: String,
}

/// A turn whose `Queued` and `UserMessage` facts already committed in the
/// daemon's atomic acceptance transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct SubmitCommittedTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
}

/// Durable actor-owned menu checkpoint reconstructed after daemon restart.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestInputCheckpoint {
    pub menu: Menu,
    pub request_seq: u64,
    pub opening_generation: u64,
    pub tool_item_id: ItemId,
    pub call_id: String,
    /// `request_input` for a blocking interactive question or nonblocking
    /// autonomous resolution, `plan` for an interrupted automatic settlement,
    /// or the mutating tool whose broker approval is waiting on the same
    /// durable menu CAS.
    pub tool_name: String,
    pub args: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitCheckpointTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub checkpoint: RequestInputCheckpoint,
}

/// Durable post-content stream interruption reconstructed after restart.
#[derive(Debug, Clone, PartialEq)]
pub struct PartialStreamCheckpoint {
    pub menu: Menu,
    pub request_seq: u64,
    pub opening_generation: u64,
    pub item_id: ItemId,
    pub text: ReplyText,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitPartialStreamTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub checkpoint: PartialStreamCheckpoint,
}

/// One incomplete text item retained across a network reconnect or daemon
/// restart. Its durable item id lets resumed deltas extend the existing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteWaitTextCheckpoint {
    pub item_id: ItemId,
    pub text: ReplyText,
}

/// One provider-authored tool call whose streamed arguments were incomplete
/// when the route disappeared. Recovery restores the accumulator and filters
/// the provider's exact replayed prefix before accepting novel argument bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteWaitToolCheckpoint {
    pub item_id: ItemId,
    pub call_id: String,
    pub name: String,
    pub args: String,
}

/// One completed local tool effect retained across a daemon restart. A
/// provider replay restores its transcript blocks without redispatching it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteWaitCompletedToolCheckpoint {
    pub call_id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub result: Option<BoundedResult>,
}

/// Durable provider-stream position for a run parked on network reachability.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RouteWaitCheckpoint {
    pub message: Option<RouteWaitTextCheckpoint>,
    pub reasoning: Option<RouteWaitTextCheckpoint>,
    pub tools: Vec<RouteWaitToolCheckpoint>,
    pub completed_tools: Vec<RouteWaitCompletedToolCheckpoint>,
    pub structured_events: Vec<StreamEvent>,
    pub response_epoch: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitRouteWaitTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub checkpoint: RouteWaitCheckpoint,
}

/// Durable coordinates for one deferred spawn tool reconstructed after a
/// daemon restart.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredToolCheckpoint {
    pub ticket: DeferredTicket,
    pub tool_item_id: ItemId,
    pub call_id: String,
    pub tool_name: String,
    pub args: String,
    pub report_emitted: bool,
    pub child_result_emitted: bool,
    pub tool_result_emitted: bool,
    pub item_completed: bool,
}

/// Durable local-child wait resumed in the same logical turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildWaitCheckpoint {
    pub tools: Vec<DeferredToolCheckpoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitChildWaitTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub checkpoint: ChildWaitCheckpoint,
}

/// Opaque dispatcher correlation for a durably established child.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeferredTicket {
    pub id: String,
    pub manifest: AgentManifest,
}

/// Terminal child data returned to the parked parent.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredToolResult {
    pub report: ChildReport,
    pub chip: ChipState,
    pub truncated: bool,
}

/// Port for general tool execution. `request_input` remains actor-owned
/// because its durable waiter is part of the turn state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDispatchResult {
    Completed(BoundedResult),
    ApprovalRequired(Menu),
    Deferred(DeferredTicket),
}

/// Durable authority decision immediately before a provider response may
/// commit `RunState::Done`.
#[derive(Debug, Clone, PartialEq)]
pub enum FinalizationGuardDecision {
    AllowDone,
    Continue {
        /// Present only for the one automatic reminder per `(graph, run)`.
        reminder: Option<String>,
    },
    ConfirmRequired(Menu),
}

#[async_trait]
pub trait FinalizationGuard: Send + Sync + std::fmt::Debug {
    async fn before_done(&self, run_id: &RunId) -> Result<FinalizationGuardDecision, HaiderError>;

    /// Request-count-aware daemon hook. Standalone guards retain their old
    /// implementation; daemon recovery persists this count at clean workflow
    /// continuation checkpoints.
    async fn before_done_after_requests(
        &self,
        run_id: &RunId,
        _provider_requests_consumed: usize,
    ) -> Result<FinalizationGuardDecision, HaiderError> {
        self.before_done(run_id).await
    }
}

/// Daemon-owned classifier for a provider-open wait that exhausted the
/// absolute provider deadline. The daemon is the only layer that knows
/// whether that deadline came from `budget.max_time_ms` or an earlier client
/// request timeout, and it commits the typed budget fact before returning a
/// replacement terminal error.
#[async_trait]
pub trait ProviderDeadlineGuard: Send + Sync + std::fmt::Debug {
    async fn map_deadline_exhausted(
        &self,
        run_id: &RunId,
    ) -> Result<Option<HaiderError>, HaiderError>;
}

/// Keeps a daemon-owned request admission lock alive until one physical
/// provider exchange reaches a terminal stream boundary.
pub struct ProviderBudgetPermit {
    _hold: Box<dyn Send>,
}

impl ProviderBudgetPermit {
    #[must_use]
    pub fn new(hold: impl Send + 'static) -> Self {
        Self {
            _hold: Box::new(hold),
        }
    }
}

/// Typed control result from the daemon-owned provider budget boundary.
/// A descendant cancelled by its root budget is lifecycle control, not a
/// provider/run failure; every other budget/store error retains its cause.
#[derive(Debug)]
pub enum ProviderBudgetGuardError {
    Cancelled,
    Failure(HaiderError),
}

impl From<HaiderError> for ProviderBudgetGuardError {
    fn from(error: HaiderError) -> Self {
        Self::Failure(error)
    }
}

/// Daemon-owned hard-budget authority at every physical provider request.
/// The preflight receives the fully shaped request and current provider
/// coordinate; usage callbacks run only after the cumulative snapshot is
/// durable, so a refusal can stop the stream at that chunk boundary.
#[async_trait]
pub trait ProviderBudgetGuard: Send + Sync + std::fmt::Debug {
    async fn before_request(
        &self,
        run_id: &RunId,
        provider: &str,
        request: &TurnRequest,
        projected_input_tokens: u64,
    ) -> Result<ProviderBudgetPermit, ProviderBudgetGuardError>;

    async fn after_usage(&self, run_id: &RunId) -> Result<(), ProviderBudgetGuardError>;

    /// Releases one admitted physical request into a confirmed route wait.
    /// The daemon retains its projection as the replaceable admission for the
    /// exact reconnect retry; it must not charge or reject the wait as missing
    /// provider usage.
    async fn after_route_interruption(
        &self,
        run_id: &RunId,
    ) -> Result<(), ProviderBudgetGuardError>;

    async fn after_request(
        &self,
        run_id: &RunId,
        provider: &str,
        model: &str,
        usage_reported: bool,
    ) -> Result<(), ProviderBudgetGuardError>;
}

async fn release_provider_budget_for_route(
    guard: Option<&Arc<dyn ProviderBudgetGuard>>,
    run_id: &RunId,
    permit: &mut Option<ProviderBudgetPermit>,
) -> Result<(), ProviderBudgetGuardError> {
    let result = if let Some(guard) = guard {
        guard.after_route_interruption(run_id).await
    } else {
        Ok(())
    };
    drop(permit.take());
    result
}

async fn release_provider_budget_request(
    guard: Option<&Arc<dyn ProviderBudgetGuard>>,
    run_id: &RunId,
    provider: &str,
    model: &str,
    usage_reported: bool,
    permit: &mut Option<ProviderBudgetPermit>,
) -> Result<(), ProviderBudgetGuardError> {
    let result = if let Some(guard) = guard {
        guard
            .after_request(run_id, provider, model, usage_reported)
            .await
    } else {
        Ok(())
    };
    drop(permit.take());
    result
}

/// A provider/account replacement for the current logical turn.
pub struct ResolvedProviderAttempt {
    pub provider: Arc<dyn Provider>,
    pub account: CredentialAlias,
    pub rotation: RotationEvent,
}

/// Why an automatic provider/model switch was required in the middle of a
/// logical turn. The reason is carried into both the durable selection
/// receipt and the UI-visible switch marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPairSwitchCause {
    FallbackChain,
    CompactionGuard,
}

impl ProviderPairSwitchCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FallbackChain => "fallback_chain",
            Self::CompactionGuard => "compaction_guard",
        }
    }
}

/// Provider-capability-derived request state resolved by the daemon for one
/// live lane. Tool names are materialized only from the actor's turn-scoped,
/// already-authorized local web definition pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderDerivedRequestState {
    pub tool_result_images_supported: bool,
    pub local_web_tool_names: Vec<String>,
    pub provider_fallback_local_web_tool_names: Vec<String>,
}

/// A fully resolved replacement lane. Daemon resolution proves the
/// credential and constructs the provider before core crosses the durable
/// switch boundary; core then installs every live request coordinate only
/// after [`ProviderPairSwitchCommitter`] confirms the metadata mutation.
#[derive(Clone)]
pub struct ProviderPairSwitchTarget {
    pub provider: Arc<dyn Provider>,
    pub account: CredentialAlias,
    pub provider_name: String,
    pub model: String,
    pub context_window: Option<u64>,
    pub cached_input_is_subset: bool,
    pub provider_request_state: ProviderDerivedRequestState,
    pub auth_scope: String,
    pub attempt_resolver: Option<Arc<dyn ProviderAttemptResolver>>,
    pub cause: ProviderPairSwitchCause,
}

impl std::fmt::Debug for ProviderPairSwitchTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderPairSwitchTarget")
            .field("account", &self.account)
            .field("provider_name", &self.provider_name)
            .field("model", &self.model)
            .field("context_window", &self.context_window)
            .field("cached_input_is_subset", &self.cached_input_is_subset)
            .field("provider_request_state", &self.provider_request_state)
            .field("auth_scope", &self.auth_scope)
            .field("cause", &self.cause)
            .finish_non_exhaustive()
    }
}

/// Secret-free coordinates committed through the daemon's existing
/// receipted `session.select_model` actor path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPairSwitch {
    pub run_id: RunId,
    pub switch_ordinal: u32,
    pub from_provider: String,
    pub from_model: String,
    pub to_provider: String,
    pub to_model: String,
    pub cause: ProviderPairSwitchCause,
}

#[async_trait]
pub trait ProviderPairSwitchCommitter: Send + Sync + std::fmt::Debug {
    async fn commit(&self, switch: &ProviderPairSwitch) -> Result<(), HaiderError>;
}

/// Result of consulting the daemon at an eligible pre-first-event failure.
pub enum ProviderAttemptDecision {
    /// Retry with refreshed credentials for the same account. This does not
    /// consume the account-rotation allowance.
    Retry {
        provider: Arc<dyn Provider>,
        account: CredentialAlias,
    },
    /// Retry this same logical request using a provider rebuilt without the
    /// rejected hosted capability; core swaps in its local-equivalent pack.
    Fallback {
        provider: Arc<dyn Provider>,
        account: CredentialAlias,
    },
    /// Commit the supplied durable event, then retry with the alternate.
    Rotate(ResolvedProviderAttempt),
    /// Commit a durable provider/model pair selection, then continue this
    /// same logical turn on the fully resolved replacement lane.
    Switch(ProviderPairSwitchTarget),
    /// Keep the existing provider and apply ordinary retry/backoff policy.
    Wait,
    /// Surface the original provider failure.
    Stop,
}

/// Provider-neutral live-credential seam. Core owns the event boundary and
/// one-hop budget; daemon implementations own account status and refresh.
#[async_trait]
pub trait ProviderAttemptResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError>;

    /// Resolves a cross-provider fallback only after the current provider is
    /// out of healthy local options. Implementations must return `Stop` for
    /// non-provider-health failures; core fences the call to the allowed
    /// health taxonomy as a second line of defense.
    async fn resolve_fallback(
        &self,
        _current_account: &CredentialAlias,
        _error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        Ok(ProviderAttemptDecision::Stop)
    }
}

/// Injectable backoff wait for the provider-retry seam (W-C M4). Production
/// installs [`RealRetrySleeper`] (a real `tokio` sleep); laws inject a sleeper
/// that returns immediately and records the requested delays, so the retry
/// schedule is asserted without any wall-clock wait. Cancellation is layered
/// OVER this by the caller — a sleeper never needs to observe the token.
#[async_trait]
pub trait RetrySleeper: Send + Sync + std::fmt::Debug {
    async fn sleep(&self, delay_ms: u64);
}

/// The production [`RetrySleeper`]: an ordinary `tokio` timer (respects paused
/// time in `#[tokio::test(start_paused = true)]` laws).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealRetrySleeper;

#[async_trait]
impl RetrySleeper for RealRetrySleeper {
    async fn sleep(&self, delay_ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
}

/// Pure exponential backoff schedule for the provider-retry seam (W-C M4):
/// `min(RETRY_CEILING_MS, RETRY_BASE_MS * 2^(attempt-1))`. `attempt` is the
/// 1-based number of the request that FAILED (attempt 1 = the original try),
/// so a first failure waits `RETRY_BASE_MS`. A present `retry_after_ms`
/// OVERRIDES this at the call site (the server's instruction wins). Being a
/// pure function of the attempt lets a law assert the exact sequence.
#[must_use]
pub fn retry_backoff_ms(attempt: usize) -> u64 {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    RETRY_BASE_MS
        .saturating_mul(2_u64.saturating_pow(exponent))
        .min(RETRY_CEILING_MS)
}

/// Stable per-run jitter in the lower half of the exponential window. This
/// avoids a reconnecting herd while keeping replayable tests deterministic.
#[must_use]
pub fn retry_jittered_backoff_ms(run_id: &RunId, attempt: usize) -> u64 {
    let base = retry_backoff_ms(attempt);
    let floor = base / 2;
    let digest =
        blake3::hash(format!("haider/provider-retry-jitter/{run_id}/{attempt}").as_bytes());
    let bytes = digest.as_bytes();
    let sample = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    floor + sample % (base.saturating_sub(floor).saturating_add(1))
}

/// Two-phase compaction port. `plan` is journaled before `compact` performs
/// private summarization and commits the final immutable overlay node.
#[derive(Debug)]
pub struct ContextCompactionRequest<'a> {
    pub run_id: &'a RunId,
    pub intent: &'a CompactionIntent,
    pub covered_messages: Vec<Message>,
    pub retained_messages: Vec<Message>,
    pub attachments: Vec<haider_provider::ResolvedAttachment>,
    pub latest_compaction_summary_end: Option<usize>,
    pub economy_before: &'a ContextEconomy,
}

#[async_trait]
pub trait ContextCompactor: Send + Sync + std::fmt::Debug {
    async fn plan(
        &self,
        run_id: &RunId,
        resume_cause: CompactionResume,
        messages: &[Message],
        current_turn_start: usize,
    ) -> Result<PlannedContextCompaction, HaiderError>;

    /// `attachments` (round 5): the replay's resolved attachments — the
    /// SAME resolution the live lane applied, so an image-bearing history
    /// replays byte-identically instead of always falling back uncached.
    /// `latest_compaction_summary_end` (hygiene round): the actor's LIVE
    /// prior-summary boundary at compact time — a second in-turn compaction
    /// must mark the FIRST compaction's summary as its replay breakpoint,
    /// not a value frozen at construction.
    async fn compact(
        &self,
        request: ContextCompactionRequest<'_>,
    ) -> Result<ContextCompactionOutcome, ContextCompactionError>;
}

/// Context compaction can encounter the same root-budget cancellation as the
/// main provider lane. Preserve that lifecycle signal across the compactor
/// port instead of flattening it into a run error.
#[derive(Debug)]
pub enum ContextCompactionError {
    Cancelled,
    Failure(HaiderError),
}

impl From<HaiderError> for ContextCompactionError {
    fn from(error: HaiderError) -> Self {
        Self::Failure(error)
    }
}

impl From<ProviderBudgetGuardError> for ContextCompactionError {
    fn from(error: ProviderBudgetGuardError) -> Self {
        match error {
            ProviderBudgetGuardError::Cancelled => Self::Cancelled,
            ProviderBudgetGuardError::Failure(error) => Self::Failure(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedContextCompaction {
    pub intent: CompactionIntent,
    /// Exclusive provider-message boundary replaced by the summary.
    pub covered_message_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextCompactionOutcome {
    pub summary: Message,
    pub economy: ContextEconomy,
}

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// Re-checks daemon-owned authority immediately before any tool route,
    /// including actor-owned request/plan/todo handling. Implementations use
    /// this narrow hook to fail closed when external session state changed
    /// after the current provider request was assembled.
    async fn preflight_tool_call(&self, _name: &str) -> Result<(), HaiderError> {
        Ok(())
    }

    async fn execute(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError>;

    /// Arc-backed production path for approval retries. Existing injected
    /// dispatchers retain the owned compatibility method; dispatchers that can
    /// consume borrowed arguments override this hook to avoid deep JSON clones.
    async fn execute_shared(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: Arc<serde_json::Value>,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.execute(
            run_id,
            item_id,
            call_id,
            name,
            args.as_ref().clone(),
            cancel,
        )
        .await
    }

    /// Returns the provider-only context snapshot for a new logical request. `None`
    /// means this dispatcher does not manage volatile context; `Some("")`
    /// clears a previously supplied snapshot. The actor calls this exactly
    /// once before each logical request and freezes the result through that
    /// request's physical transport retries. State changed by a tool round is
    /// therefore visible at the next provider-request boundary.
    async fn refresh_volatile_context_tail(&self) -> Result<Option<String>, HaiderError> {
        Ok(None)
    }

    /// Activates work owned by a newly committed approval checkpoint.
    ///
    /// The menu and `PermissionRequired` state are durable before this hook is
    /// called. Implementations may therefore poll an external gate and answer
    /// the menu through the ordinary CAS without creating an in-flight tool
    /// effect. The same hook is called after restart with the reconstructed
    /// checkpoint, so activation must be idempotent.
    async fn activate_approval(
        &self,
        _run_id: &RunId,
        _checkpoint: &RequestInputCheckpoint,
    ) -> Result<(), HaiderError> {
        Ok(())
    }

    /// Applies a permission answer only after the actor has observed the
    /// daemon CAS's committed `MenuAnswered` envelope.
    async fn resolve_approval(
        &self,
        _menu: &Menu,
        _answer: &MenuAnswer,
    ) -> Result<(), HaiderError> {
        Err(HaiderError::new(
            ErrorCode::PermissionDenied,
            "tool dispatcher does not support approval menus",
            false,
        ))
    }

    /// Waits for one previously established deferred child. Implementations
    /// must return a terminal report for child success, failure, or
    /// cancellation; cancellation of the parent is signalled separately by
    /// `cancel`.
    async fn collect_deferred(
        &self,
        _ticket: &DeferredTicket,
        _cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::Internal,
            "tool dispatcher does not support deferred collection",
            false,
        ))
    }

    /// Marks a delivered deferred result collected after the parent tool
    /// result and item completion are durable.
    async fn acknowledge_deferred(&self, _ticket: &DeferredTicket) -> Result<(), HaiderError> {
        Ok(())
    }

    /// Cancels every child still owned by this turn. The actor invokes this
    /// only on a real turn cancellation, never when a durable child-wait
    /// checkpoint is quietly parked for restart.
    async fn cancel_outstanding_deferred(&self) -> Result<(), HaiderError> {
        Ok(())
    }

    /// Cancels and drains effects abandoned by an orderly turn cancellation.
    async fn cancel(&self) -> Result<(), HaiderError> {
        self.close().await
    }

    /// Drains process/finalizer ownership after the logical turn ends.
    async fn close(&self) -> Result<(), HaiderError> {
        Ok(())
    }
}

impl SubmitTurn {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Cooperative cancellation signal shared by everything driving one turn.
#[derive(Debug, Clone)]
pub struct CancelToken {
    /// Single source of truth: the watch value IS the cancelled flag.
    flag: watch::Sender<bool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        let (flag, _receiver) = watch::channel(false);
        Self { flag }
    }

    /// Idempotent; wakes every pending [`cancelled`](Self::cancelled) wait.
    pub fn cancel(&self) {
        self.flag.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.flag.borrow()
    }

    /// Resolves once `cancel` has been called — immediately if it already was.
    pub async fn cancelled(&self) {
        let mut receiver = self.flag.subscribe();
        // Never errors: `self` keeps the sender alive for the whole wait.
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// Terminal report for one turn. `error` is set only for `Errored`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub state: RunState,
    pub finish_reason: FinishReason,
    pub error: Option<HaiderError>,
}

/// Caller's grip on one accepted turn: cancel it and/or await its outcome.
#[derive(Debug)]
pub struct TurnHandle {
    cancel: CancelToken,
    outcome: oneshot::Receiver<TurnOutcome>,
}

impl TurnHandle {
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub async fn wait(self) -> Result<TurnOutcome, HaiderError> {
        self.outcome.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session actor stopped before reporting the turn outcome",
                true,
            )
        })
    }
}

/// Cloneable command and subscription surface for a running actor.
#[derive(Debug, Clone)]
pub struct HarnessHandle {
    commands: mpsc::Sender<ActorCommand>,
    events: broadcast::Sender<RawEnvelope>,
    committed_batches: broadcast::Sender<Arc<[RawEnvelope]>>,
    state: watch::Receiver<Option<RunState>>,
    committed_menus: watch::Sender<Option<RawEnvelope>>,
    provider_retry_wake: Arc<ProviderRetryWake>,
    promoted_steers: Arc<PromotedSteerMailbox>,
}

impl HarnessHandle {
    /// Queues a turn; the actor runs turns strictly one at a time, in order.
    pub async fn submit_turn(&self, request: SubmitTurn) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::Local(request)).await
    }

    /// Starts a daemon-accepted turn without duplicating its durable
    /// `Queued`/`UserMessage` prefix.
    pub async fn submit_committed_turn(
        &self,
        request: SubmitCommittedTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::Committed(request)).await
    }

    pub async fn submit_checkpoint_turn(
        &self,
        request: SubmitCheckpointTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::Checkpoint(Box::new(request)))
            .await
    }

    pub async fn submit_partial_stream_turn(
        &self,
        request: SubmitPartialStreamTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::PartialStream(Box::new(request)))
            .await
    }

    pub async fn submit_route_wait_turn(
        &self,
        request: SubmitRouteWaitTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::RouteWait(Box::new(request)))
            .await
    }

    pub async fn submit_child_wait_turn(
        &self,
        request: SubmitChildWaitTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::ChildWait(Box::new(request)))
            .await
    }

    /// Queues a daemon-authored steer for the active logical turn.
    ///
    /// Delivery is deliberately nonblocking: the actor records the text for
    /// the next provider-request boundary, while a provider/tool that never
    /// reaches such a boundary remains cancellable by its supervisor.
    pub fn nudge(&self, text: impl Into<String>) -> Result<(), HaiderError> {
        self.deliver_mid_turn(text.into(), DeliveryMode::Steer)
    }

    /// Queues user input for the next resolved tool-call boundary. The
    /// pending call is held before dispatch and the provider is re-prompted
    /// with this input so it can revise or confirm the call first.
    pub fn subturn(&self, text: impl Into<String>) -> Result<(), HaiderError> {
        self.deliver_mid_turn(text.into(), DeliveryMode::Subturn)
    }

    /// Reserves one daemon-promoted steer before its durable queue mutation.
    ///
    /// The reservation and the turn's final `Done` fence share one mutex:
    /// either this call wins and terminalization waits for `commit`/drop, or
    /// terminalization wins and the queue mutation can be refused untouched.
    pub fn reserve_promoted_steer(
        &self,
        text: impl Into<String>,
    ) -> Result<PromotedSteerReservation, HaiderError> {
        self.promoted_steers
            .reserve(text.into(), self.commands.clone())
    }

    fn deliver_mid_turn(&self, text: String, mode: DeliveryMode) -> Result<(), HaiderError> {
        self.commands
            .try_send(ActorCommand::Nudge { text, mode })
            .map_err(|error| {
                HaiderError::new(
                    ErrorCode::Busy,
                    format!("session actor could not accept mid-turn input: {error}"),
                    true,
                )
            })
    }

    async fn submit(&self, request: TurnSubmission) -> Result<TurnHandle, HaiderError> {
        let (accepted, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Submit { request, accepted })
            .await
            .map_err(|_| {
                HaiderError::new(ErrorCode::Internal, "session actor is not running", true)
            })?;
        response.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session actor stopped before accepting the turn",
                true,
            )
        })?
    }

    /// Requests an explicit actor stop. If a turn is active its cancellation
    /// terminalizes before the actor acknowledges and exits.
    pub async fn stop(&self) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Stop { completed })
            .await
            .map_err(|_| {
                HaiderError::new(ErrorCode::Internal, "session actor is not running", true)
            })?;
        response.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session actor stopped before acknowledging shutdown",
                true,
            )
        })
    }

    /// Answers the currently open input menu. Invalid or stale answers fail
    /// without closing the menu, so another surface may still answer it.
    pub async fn answer_menu(&self, answer: MenuAnswer) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::AnswerMenu { answer, completed })
            .await
            .map_err(|_| {
                HaiderError::new(ErrorCode::Internal, "session actor is not running", true)
            })?;
        response.await.map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session actor stopped before resolving the menu answer",
                true,
            )
        })?
    }

    /// Wakes a pending menu from an answer envelope that another durable
    /// authority already committed.
    ///
    /// The harness must not append the answer again. This nonblocking watch
    /// edge is intentionally separate from the bounded command queue: one
    /// menu can have only one authoritative resolution.
    pub fn apply_committed_menu_event(&self, envelope: RawEnvelope) -> Result<(), HaiderError> {
        if !envelope.payload.decode_event().is_ok_and(|payload| {
            matches!(
                payload,
                EventPayload::MenuAnswered(_) | EventPayload::MenuClosed { .. }
            )
        }) {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "committed menu wake must carry MenuAnswered or MenuClosed",
                false,
            ));
        }
        self.committed_menus.send(Some(envelope)).map_err(|_| {
            HaiderError::new(
                ErrorCode::Internal,
                "session actor stopped before applying the committed menu answer",
                true,
            )
        })
    }

    /// Live feed of committed envelopes (from subscription time onward).
    pub fn subscribe(&self) -> broadcast::Receiver<RawEnvelope> {
        self.events.subscribe()
    }

    /// Live feed of committed batches. Each batch is the exact owned slice
    /// returned by the durability seam, shared without cloning its payloads.
    pub fn subscribe_committed_batches(&self) -> broadcast::Receiver<Arc<[RawEnvelope]>> {
        self.committed_batches.subscribe()
    }

    pub fn current_state(&self) -> Option<RunState> {
        self.state.borrow().clone()
    }

    pub fn state_receiver(&self) -> watch::Receiver<Option<RunState>> {
        self.state.clone()
    }

    /// Short-circuits the exact durable provider backoff named by
    /// `retrying_event_id`. The command id is consumed once for the lifetime
    /// of this actor, so receipt replay is an idempotent no-op and cannot wake
    /// a later retry ladder that happens to show the same attempt number.
    ///
    /// Returns `true` only when this call changed the armed wait to fired.
    #[must_use]
    pub fn wake_provider_retry(
        &self,
        command_id: impl Into<String>,
        retrying_event_id: &EventId,
    ) -> bool {
        self.provider_retry_wake
            .wake(command_id.into(), retrying_event_id)
    }
}

enum ActorCommand {
    Submit {
        request: TurnSubmission,
        accepted: oneshot::Sender<Result<TurnHandle, HaiderError>>,
    },
    AnswerMenu {
        answer: MenuAnswer,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Nudge {
        text: String,
        mode: DeliveryMode,
    },
    PromotedSteerWake {
        reservation_id: u64,
    },
    Stop {
        completed: oneshot::Sender<()>,
    },
}

/// Prepared half of a queue promotion. Dropping it aborts without exposing
/// text to the provider; committing it makes the durable steer available at
/// the next safe provider boundary.
#[derive(Debug)]
pub struct PromotedSteerReservation {
    id: u64,
    mailbox: Arc<PromotedSteerMailbox>,
    commands: mpsc::Sender<ActorCommand>,
    resolved: bool,
}

impl PromotedSteerReservation {
    pub fn commit(mut self) -> Result<(), HaiderError> {
        self.mailbox.commit(self.id)?;
        self.resolved = true;
        // A full command lane only delays delivery until the mandatory
        // terminal fence drains the mailbox; durable text is never dropped.
        let _ = self.commands.try_send(ActorCommand::PromotedSteerWake {
            reservation_id: self.id,
        });
        Ok(())
    }
}

impl Drop for PromotedSteerReservation {
    fn drop(&mut self) {
        if !self.resolved {
            self.mailbox.abort(self.id);
        }
    }
}

enum TurnSubmission {
    Local(SubmitTurn),
    Committed(SubmitCommittedTurn),
    Checkpoint(Box<SubmitCheckpointTurn>),
    PartialStream(Box<SubmitPartialStreamTurn>),
    RouteWait(Box<SubmitRouteWaitTurn>),
    ChildWait(Box<SubmitChildWaitTurn>),
}

enum MenuWake {
    Command(ActorCommand),
    Committed(RawEnvelope),
}

/// One actor-owned provider-backoff wake latch.
///
/// `Notify` supplies the pending-future wake edge, while the mutex state is
/// the source of truth: it retains a wake that arrives before the future is
/// first polled, coalesces distinct commands for one backoff, and remembers
/// command ids across later backoffs so receipt replay cannot fire twice.
#[derive(Debug, Default)]
struct ProviderRetryWake {
    state: Mutex<ProviderRetryWakeState>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct ProviderRetryWakeState {
    armed_event_id: Option<EventId>,
    fired: bool,
    consumed_commands: HashSet<String>,
}

/// Volatile request phase used only to classify an absolute-deadline terminal.
///
/// Durable `Retrying`/provider `Waiting` states cover the backoff itself. This
/// latch covers the small retry-admission intervals on either side of those
/// facts: deciding whether to retry and reaching the next provider future.
/// The provider future clears it on its first poll, so a deadline produced
/// before that poll remains retry exhaustion rather than an in-flight timeout.
#[derive(Debug, Default)]
struct ProviderDeadlineState {
    retry_admission: AtomicBool,
}

impl ProviderDeadlineState {
    fn begin_retry_admission(&self) {
        self.retry_admission.store(true, Ordering::Release);
    }

    fn begin_provider_request(&self) {
        self.retry_admission.store(false, Ordering::Release);
    }

    fn retry_admission_in_progress(&self) -> bool {
        self.retry_admission.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default)]
struct PromotedSteerMailbox {
    state: Mutex<PromotedSteerMailboxState>,
    changed: Notify,
}

#[derive(Debug, Default)]
struct PromotedSteerMailboxState {
    accepting: bool,
    next_id: u64,
    reserved: HashMap<u64, String>,
    committed: VecDeque<(u64, String)>,
}

impl PromotedSteerMailbox {
    fn state(&self) -> std::sync::MutexGuard<'_, PromotedSteerMailboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin_turn(&self) {
        let mut state = self.state();
        state.accepting = true;
        state.reserved.clear();
        state.committed.clear();
    }

    fn close_turn(&self) {
        let mut state = self.state();
        state.accepting = false;
        state.reserved.clear();
        state.committed.clear();
        drop(state);
        self.changed.notify_waiters();
    }

    fn reserve(
        self: &Arc<Self>,
        text: String,
        commands: mpsc::Sender<ActorCommand>,
    ) -> Result<PromotedSteerReservation, HaiderError> {
        let id = {
            let mut state = self.state();
            if !state.accepting {
                return Err(HaiderError::new(
                    ErrorCode::RunNotActive,
                    "active turn crossed its promotion boundary",
                    false,
                ));
            }
            state.next_id = state.next_id.checked_add(1).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Busy,
                    "promoted steer reservation space is exhausted",
                    true,
                )
            })?;
            let id = state.next_id;
            state.reserved.insert(id, text);
            id
        };
        Ok(PromotedSteerReservation {
            id,
            mailbox: Arc::clone(self),
            commands,
            resolved: false,
        })
    }

    fn commit(&self, id: u64) -> Result<(), HaiderError> {
        let mut state = self.state();
        let text = state.reserved.remove(&id).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::RunNotActive,
                "promoted steer reservation is no longer active",
                false,
            )
        })?;
        state.committed.push_back((id, text));
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn abort(&self, id: u64) {
        let removed = self.state().reserved.remove(&id).is_some();
        if removed {
            self.changed.notify_waiters();
        }
    }

    fn take_committed(&self, id: u64) -> Option<String> {
        let mut state = self.state();
        let position = state
            .committed
            .iter()
            .position(|(candidate, _)| *candidate == id)?;
        state.committed.remove(position).map(|(_, text)| text)
    }

    fn drain_committed(&self) -> Vec<String> {
        self.state()
            .committed
            .drain(..)
            .map(|(_, text)| text)
            .collect()
    }

    /// Completes the terminal promotion fence without waiting when every
    /// reservation has already resolved. `None` means a caller must preserve
    /// any pending durable facts before awaiting [`Self::finish_boundary`].
    fn try_finish_boundary(&self) -> Option<Vec<String>> {
        let mut state = self.state();
        if !state.committed.is_empty() {
            return Some(state.committed.drain(..).map(|(_, text)| text).collect());
        }
        if state.reserved.is_empty() {
            state.accepting = false;
            return Some(Vec::new());
        }
        None
    }

    async fn finish_boundary(&self) -> Vec<String> {
        loop {
            // Register before inspecting the reservation set so commit/drop
            // cannot land between the predicate and the wait.
            let changed = self.changed.notified();
            if let Some(committed) = self.try_finish_boundary() {
                return committed;
            }
            changed.await;
        }
    }
}

impl ProviderRetryWake {
    fn state(&self) -> std::sync::MutexGuard<'_, ProviderRetryWakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arm(&self, retrying_event_id: EventId) {
        let mut state = self.state();
        debug_assert!(state.armed_event_id.is_none());
        state.armed_event_id = Some(retrying_event_id);
        state.fired = false;
    }

    fn wake(&self, command_id: String, retrying_event_id: &EventId) -> bool {
        let fired = {
            let mut state = self.state();
            if !state.consumed_commands.insert(command_id)
                || state.armed_event_id.as_ref() != Some(retrying_event_id)
                || state.fired
            {
                false
            } else {
                state.fired = true;
                true
            }
        };
        if fired {
            self.notify.notify_one();
        }
        fired
    }

    async fn fired(&self, retrying_event_id: &EventId) {
        loop {
            // Register before inspecting the latch so a concurrent wake
            // cannot fall between the predicate check and waiter creation.
            let notified = self.notify.notified();
            let fired = {
                let state = self.state();
                state.armed_event_id.as_ref() == Some(retrying_event_id) && state.fired
            };
            if fired {
                return;
            }
            notified.await;
        }
    }

    fn disarm(&self, retrying_event_id: &EventId) {
        let mut state = self.state();
        if state.armed_event_id.as_ref() == Some(retrying_event_id) {
            state.armed_event_id = None;
            state.fired = false;
        }
    }
}

/// Single-session, single-writer run loop.
pub struct HarnessActor {
    config: HarnessConfig,
    provider: Arc<dyn Provider>,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    store: Arc<dyn StoreHandle>,
    artifact_reader: Option<Arc<dyn ArtifactReader>>,
    resolved_tool_images: HashMap<ArtifactRef, ResolvedAttachment>,
    validated_tool_image_refs: HashMap<ArtifactRef, ImageBlockRef>,
    commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RawEnvelope>,
    committed_batches: broadcast::Sender<Arc<[RawEnvelope]>>,
    state: watch::Sender<Option<RunState>>,
    committed_menus: watch::Receiver<Option<RawEnvelope>>,
    provider_retry_wake: Arc<ProviderRetryWake>,
    provider_deadline_state: Arc<ProviderDeadlineState>,
    promoted_steers: Arc<PromotedSteerMailbox>,
    next_run: u64,
    event_ids: Arc<EventIdGenerator>,
    /// Actor start instant (ms) — embedded in event ids for global uniqueness.
    started_at_ms: u64,
    next_item: u64,
    next_node: u64,
    next_menu: u64,
    tree_head_initialized: bool,
    tree_head: Option<NodeId>,
    deferred_commands: VecDeque<ActorCommand>,
    pending_nudges: Vec<String>,
    pending_subturns: Vec<String>,
    /// G1: the OPEN `todo_write` plan lifecycle. One `TurnItem::Plan` item id
    /// per lifecycle: the first write of a run Starts it, later writes emit
    /// Completed (replace semantics) under the same id, and completion or an
    /// empty-list clear closes it (a later write starts a fresh id — the
    /// projection closes finished item ids forever). Keyed by run so a stale
    /// lifecycle from an earlier run never leaks into the next one.
    plan: Option<PlanLifecycle>,
    /// One contiguous provider-stream delta held before its timed or semantic
    /// flush. Keeping one entry, rather than a map, preserves event ordering
    /// when providers interleave item kinds.
    pending_item_delta: Option<PendingItemDelta>,
    pending_item_delta_deadline: Option<tokio::time::Instant>,
}

/// See [`HarnessActor::plan`].
struct PlanLifecycle {
    run_id: RunId,
    item_id: ItemId,
}

impl HarnessActor {
    pub fn new(
        config: HarnessConfig,
        provider: Arc<dyn Provider>,
        store: Arc<dyn StoreHandle>,
    ) -> (Self, HarnessHandle) {
        Self::new_with_dispatcher(config, provider, store, None)
    }

    pub fn new_with_dispatcher(
        config: HarnessConfig,
        provider: Arc<dyn Provider>,
        store: Arc<dyn StoreHandle>,
        dispatcher: Option<Arc<dyn ToolDispatcher>>,
    ) -> (Self, HarnessHandle) {
        Self::new_with_dispatcher_and_artifacts(config, provider, store, dispatcher, None)
    }

    /// Additive daemon seam for resolving CAS-backed images that appear only
    /// after a tool runs. Existing embedders retain the text/placeholder path.
    pub fn new_with_dispatcher_and_artifacts(
        config: HarnessConfig,
        provider: Arc<dyn Provider>,
        store: Arc<dyn StoreHandle>,
        dispatcher: Option<Arc<dyn ToolDispatcher>>,
        artifact_reader: Option<Arc<dyn ArtifactReader>>,
    ) -> (Self, HarnessHandle) {
        let started_at_ms = config.started_at_ms.unwrap_or_else(unix_time_ms);
        let event_ids = config.event_ids.clone().unwrap_or_else(|| {
            Arc::new(EventIdGenerator::new(format!(
                "evt-{}-{}-{}",
                config.session_id, config.worker_generation, started_at_ms
            )))
        });
        let (command_sender, commands) = mpsc::channel(config.command_capacity.max(1));
        let (events, _) = broadcast::channel(config.broadcast_capacity.max(1));
        let (committed_batches, _) = broadcast::channel(config.broadcast_capacity.max(1));
        let (state, state_receiver) = watch::channel(None);
        let (committed_menus, committed_menu_receiver) = watch::channel(None);
        let provider_retry_wake = Arc::new(ProviderRetryWake::default());
        let provider_deadline_state = Arc::new(ProviderDeadlineState::default());
        let promoted_steers = Arc::new(PromotedSteerMailbox::default());
        let handle = HarnessHandle {
            commands: command_sender,
            events: events.clone(),
            committed_batches: committed_batches.clone(),
            state: state_receiver,
            committed_menus,
            provider_retry_wake: Arc::clone(&provider_retry_wake),
            promoted_steers: Arc::clone(&promoted_steers),
        };
        (
            Self {
                config,
                provider,
                dispatcher,
                store,
                artifact_reader,
                resolved_tool_images: HashMap::new(),
                validated_tool_image_refs: HashMap::new(),
                commands,
                events,
                committed_batches,
                state,
                committed_menus: committed_menu_receiver,
                provider_retry_wake,
                provider_deadline_state,
                promoted_steers,
                next_run: 0,
                event_ids,
                started_at_ms,
                next_item: 0,
                next_node: 0,
                next_menu: 0,
                tree_head_initialized: false,
                tree_head: None,
                deferred_commands: VecDeque::new(),
                pending_nudges: Vec::new(),
                pending_subturns: Vec::new(),
                plan: None,
                pending_item_delta: None,
                pending_item_delta_deadline: None,
            },
            handle,
        )
    }

    async fn resolve_tool_result_images(
        &mut self,
        messages: &mut [Message],
    ) -> Result<Vec<ResolvedAttachment>, HaiderError> {
        let mut attachments = self.config.attachments.clone();
        let images_supported = self.config.tool_result_images_supported;

        let mut all_requested = Vec::<ImageBlockRef>::new();
        for image in messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::ToolResult { images, .. } => Some(images.as_slice()),
                _ => None,
            })
            .flatten()
        {
            if !matches!(image.media_type.as_str(), "image/png" | "image/jpeg")
                || image.width == 0
                || image.height == 0
                || image.width > TOOL_RESULT_IMAGE_MAX_DIMENSION
                || image.height > TOOL_RESULT_IMAGE_MAX_DIMENSION
                || image.byte_len == 0
                || image.byte_len > TOOL_RESULT_IMAGE_MAX_BYTES
            {
                return Err(tool_image_corrupt(format!(
                    "tool image {} carries invalid bounded metadata",
                    image.artifact
                )));
            }
            if let Some(existing) = all_requested
                .iter()
                .find(|existing| existing.artifact == image.artifact)
            {
                if existing != image {
                    return Err(tool_image_corrupt(format!(
                        "tool image {} has conflicting metadata",
                        image.artifact
                    )));
                }
            } else {
                all_requested.push(image.clone());
            }
        }

        let reader = self.artifact_reader.as_ref().map(Arc::clone);
        if let Some(reader) = &reader {
            for image in &all_requested {
                if let Some(existing) = self.validated_tool_image_refs.get(&image.artifact) {
                    if existing != image {
                        return Err(tool_image_corrupt(format!(
                            "tool image {} has conflicting metadata",
                            image.artifact
                        )));
                    }
                    continue;
                }
                let bytes = reader.read_artifact(&image.artifact).await.map_err(|_| {
                    tool_image_corrupt(format!(
                        "tool image {} is missing from the CAS",
                        image.artifact
                    ))
                })?;
                haider_store::validate_image_block(&bytes, image).map_err(|_| {
                    tool_image_corrupt(format!(
                        "tool image {} does not match its bounded CAS metadata",
                        image.artifact
                    ))
                })?;
                self.validated_tool_image_refs
                    .insert(image.artifact.clone(), image.clone());
            }
        } else if images_supported && !all_requested.is_empty() {
            return Err(tool_image_corrupt(
                "image-bearing provider context has no CAS artifact reader",
            ));
        }

        apply_tool_result_image_budget(messages);
        if !images_supported {
            degrade_tool_result_images_to_placeholders(messages);
            return Ok(attachments);
        }

        let mut requested = Vec::<ImageBlockRef>::new();
        for image in messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::ToolResult { images, .. } => Some(images.as_slice()),
                _ => None,
            })
            .flatten()
        {
            if !requested
                .iter()
                .any(|requested| requested.artifact == image.artifact)
            {
                requested.push(image.clone());
            }
        }
        if requested.is_empty() {
            self.resolved_tool_images.clear();
            return Ok(attachments);
        }
        let Some(reader) = reader else {
            return Err(tool_image_corrupt(
                "image-bearing provider context has no CAS artifact reader",
            ));
        };
        self.resolved_tool_images
            .retain(|artifact, _| requested.iter().any(|image| image.artifact == *artifact));
        for image in &requested {
            if self.resolved_tool_images.contains_key(&image.artifact) {
                continue;
            }
            let bytes = reader.read_artifact(&image.artifact).await.map_err(|_| {
                tool_image_corrupt(format!(
                    "tool image {} is missing from the CAS",
                    image.artifact
                ))
            })?;
            haider_store::validate_image_block(&bytes, image).map_err(|_| {
                tool_image_corrupt(format!(
                    "tool image {} does not match its bounded CAS metadata",
                    image.artifact
                ))
            })?;
            self.validated_tool_image_refs
                .insert(image.artifact.clone(), image.clone());
            self.resolved_tool_images.insert(
                image.artifact.clone(),
                ResolvedAttachment {
                    artifact: image.artifact.clone(),
                    data_base64: BASE64.encode(bytes),
                },
            );
        }
        for image in requested {
            if attachments
                .iter()
                .any(|attachment| attachment.artifact == image.artifact)
            {
                continue;
            }
            if let Some(resolved) = self.resolved_tool_images.get(&image.artifact) {
                attachments.push(resolved.clone());
            }
        }
        Ok(attachments)
    }

    /// Validates every tool-produced image before its ref can enter the
    /// journal. This is capability- and context-budget-independent: even an
    /// image omitted from the next provider request must be an honest CAS
    /// object with exact metadata.
    async fn admit_tool_result_images(
        &mut self,
        images: &[ImageBlockRef],
    ) -> Result<(), DriveError> {
        if images.is_empty() {
            return Ok(());
        }
        let Some(reader) = self.artifact_reader.as_ref().map(Arc::clone) else {
            return Err(DriveError::Store(tool_image_corrupt(
                "an image-bearing tool result has no CAS artifact reader",
            )));
        };
        for image in images {
            if let Some(existing) = self.validated_tool_image_refs.get(&image.artifact) {
                if existing != image {
                    return Err(DriveError::Store(tool_image_corrupt(format!(
                        "tool image {} has conflicting metadata",
                        image.artifact
                    ))));
                }
                continue;
            }
            let bytes = reader.read_artifact(&image.artifact).await.map_err(|_| {
                DriveError::Store(tool_image_corrupt(format!(
                    "tool image {} is missing from the CAS",
                    image.artifact
                )))
            })?;
            haider_store::validate_image_block(&bytes, image).map_err(|_| {
                DriveError::Store(tool_image_corrupt(format!(
                    "tool image {} does not match its bounded CAS metadata",
                    image.artifact
                )))
            })?;
            self.validated_tool_image_refs
                .insert(image.artifact.clone(), image.clone());
        }
        Ok(())
    }

    /// Spawns [`run`](Self::run) detached; the loop exits (and the task ends)
    /// once every clone of the returned handle is dropped.
    pub fn spawn(
        config: HarnessConfig,
        provider: Arc<dyn Provider>,
        store: Arc<dyn StoreHandle>,
    ) -> HarnessHandle {
        let (actor, handle) = Self::new(config, provider, store);
        let _task = tokio::spawn(actor.run());
        handle
    }

    /// Processes submissions strictly in order until every handle is dropped.
    pub async fn run(mut self) {
        while let Some(command) = self.next_command().await {
            match command {
                ActorCommand::Submit { request, accepted } => {
                    let cancel = CancelToken::new();
                    let (outcome_sender, outcome) = oneshot::channel();
                    let turn = TurnHandle {
                        cancel: cancel.clone(),
                        outcome,
                    };
                    if accepted.send(Ok(turn)).is_err() {
                        // Submitter vanished before receiving the handle;
                        // drop the turn un-run rather than run it unowned.
                        continue;
                    }
                    self.promoted_steers.begin_turn();
                    let outcome = self.drive_turn(request, cancel).await;
                    self.promoted_steers.close_turn();
                    let _ = outcome_sender.send(outcome);
                }
                ActorCommand::AnswerMenu { completed, .. } => {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::MenuNotFound,
                        "there is no open input menu",
                        false,
                    )));
                }
                ActorCommand::Nudge { .. } => {
                    // The target turn crossed its terminal boundary before
                    // this command was observed. Durable run state wins; a
                    // stale nudge must not create a new logical turn.
                }
                ActorCommand::PromotedSteerWake { .. } => {
                    // The finalization fence already closed this turn. A
                    // committed promotion remains durable for recovery.
                }
                ActorCommand::Stop { completed } => {
                    let _ = completed.send(());
                    break;
                }
            }
        }
        // A normal stop arrives only after an active turn's terminal path has
        // flushed its delta, but retain this drain barrier for channel-close
        // and future control paths.
        if let Err(error) = self.flush_pending_item_delta().await {
            tracing::error!(
                ?error,
                "session actor shutdown could not flush a buffered delta"
            );
        }
    }

    /// Runs one turn to a terminal state. Every return path commits that
    /// terminal `RunState` (best effort) before reporting the outcome.
    async fn drive_turn(&mut self, submit: TurnSubmission, cancel: CancelToken) -> TurnOutcome {
        // A subturn belongs only to the active logical turn. Cancellation or
        // failure may end that turn before a boundary; never leak its input
        // into a later queued turn.
        self.pending_subturns.clear();
        self.provider_deadline_state.begin_provider_request();
        // Tool definitions are a provider cache ABI. Freeze their order and
        // recursively canonicalize schemas once at the conversation-store
        // boundary so standalone harnesses receive the same stability as
        // daemon-built turns.
        if self.config.shared_tools.is_none() {
            self.config.tools = canonical_tool_definitions(&self.config.tools);
        }
        if self.config.shared_provider_tool_fallback_tools.is_none() {
            self.config.provider_tool_fallback_tools =
                canonical_tool_definitions(&self.config.provider_tool_fallback_tools);
        }
        let (run_id, mut messages, checkpoint, partial_stream, mut route_wait, child_wait) =
            match submit {
                TurnSubmission::Local(submit) => {
                    let run_id = self.next_run_id();
                    if let Err(error) = self.commit_state(&run_id, RunState::Queued).await {
                        return self.errored_state_outcome(&run_id, error).await;
                    }
                    if let Err(error) = self
                        .commit_tree_fragment(
                            &run_id,
                            EventPayload::UserMessage {
                                text: submit.text.clone(),
                                attachments: Vec::new(),
                                mode: DeliveryMode::Steer,
                            },
                            prompt_verbatim_render(),
                            NodeKind::UserTurn {
                                text: submit.text.clone(),
                                attachments: Vec::new(),
                            },
                        )
                        .await
                    {
                        return self.errored_state_outcome(&run_id, error).await;
                    }
                    (
                        run_id,
                        vec![Message::user_text(submit.text)],
                        None,
                        None,
                        None,
                        None,
                    )
                }
                TurnSubmission::Committed(submit) => {
                    (submit.run_id, submit.messages, None, None, None, None)
                }
                TurnSubmission::Checkpoint(submit) => {
                    let submit = *submit;
                    (
                        submit.run_id,
                        submit.messages,
                        Some(submit.checkpoint),
                        None,
                        None,
                        None,
                    )
                }
                TurnSubmission::PartialStream(submit) => {
                    let submit = *submit;
                    (
                        submit.run_id,
                        submit.messages,
                        None,
                        Some(submit.checkpoint),
                        None,
                        None,
                    )
                }
                TurnSubmission::RouteWait(submit) => {
                    let submit = *submit;
                    (
                        submit.run_id,
                        submit.messages,
                        None,
                        None,
                        Some(submit.checkpoint),
                        None,
                    )
                }
                TurnSubmission::ChildWait(submit) => {
                    let submit = *submit;
                    (
                        submit.run_id,
                        submit.messages,
                        None,
                        None,
                        None,
                        Some(submit.checkpoint),
                    )
                }
            };
        let restore_budget = checkpoint.is_some()
            || partial_stream.is_some()
            || route_wait.is_some()
            || child_wait.is_some()
            || self.config.provider_requests_already_made > 0
            || self.config.provider_request_ordinal_already_made > 0;
        // The compiler always places the accepted current user message last.
        // Everything before it is the only prefix eligible for mid-turn
        // forced compaction; current-run content remains a verbatim suffix.
        let structural_current_turn_start = messages.len().saturating_sub(1);
        let mut current_turn_start = self
            .config
            .cache_current_user_start
            .filter(|boundary| *boundary <= structural_current_turn_start)
            .unwrap_or(structural_current_turn_start);
        let mut stable_history_end = self
            .config
            .cache_stable_history_end
            .filter(|boundary| *boundary <= current_turn_start)
            .unwrap_or(current_turn_start);
        let mut latest_compaction_summary_end = self
            .config
            .cache_compaction_summary_end
            .filter(|boundary| *boundary <= stable_history_end);
        // Provider-only context remains separate from the reconstructed
        // conversation. It is refreshed at each logical request boundary and
        // frozen across only that request's physical transport retries. This
        // lets a workflow rebind after durable stage progress while keeping
        // the snapshot out of durable prompt history.
        let mut volatile_user_tail = self.config.volatile_user_tail.take();

        let mut message: Option<TextAccumulator> = None;
        let mut reasoning: Option<TextAccumulator> = None;
        let mut tools: Vec<ToolAccumulator> = Vec::new();
        // Recovery checkpoints carry canonical execution names. Recover the
        // original spelling from the already-durable provider Start markers
        // only on resume; ordinary turns perform no extra journal reads.
        let mut recovered_names = HashMap::new();
        let mut malformed_tool_pending_repair = false;
        let recovery_calls: HashSet<&str> =
            checkpoint
                .iter()
                .map(|tool| tool.call_id.as_str())
                .chain(route_wait.iter().flat_map(|checkpoint| {
                    checkpoint.tools.iter().map(|tool| tool.call_id.as_str())
                }))
                .chain(child_wait.iter().flat_map(|checkpoint| {
                    checkpoint.tools.iter().map(|tool| tool.call_id.as_str())
                }))
                .collect();
        if checkpoint.is_some()
            || partial_stream.is_some()
            || route_wait.is_some()
            || child_wait.is_some()
            || self.config.provider_requests_already_made > 0
        {
            (recovered_names, malformed_tool_pending_repair) = match self
                .recover_tool_repair_state(&run_id, &recovery_calls)
                .await
            {
                Ok(state) => state,
                // Checkpoint items have not been reconstructed yet. Leave the
                // durable run recoverable rather than seal it with open items.
                Err(error) => return errored_outcome(error),
            };
        }
        let mut replay = ReplayPrefix::default();
        let mut route_message_ranges = VecDeque::<ReplyText>::new();
        if let Some(checkpoint) = route_wait.as_mut() {
            replay.response_epoch = checkpoint.response_epoch;
            if let Some(checkpoint) = checkpoint.message.as_ref() {
                message = Some(TextAccumulator::from_shared(
                    checkpoint.item_id.clone(),
                    &checkpoint.text,
                    true,
                ));
                replay.message_applied = Some(checkpoint.text.clone());
            }
            if let Some(checkpoint) = checkpoint.reasoning.as_ref() {
                reasoning = Some(TextAccumulator::from_shared(
                    checkpoint.item_id.clone(),
                    &checkpoint.text,
                    false,
                ));
                replay.reasoning_applied = Some(checkpoint.text.clone());
            }
            for tool in &checkpoint.tools {
                tools.push(ToolAccumulator {
                    item_id: tool.item_id.clone(),
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                    args: tool.args.clone(),
                    requested_name: recovered_names.remove(&tool.call_id),
                    parsed_args: OnceLock::new(),
                });
                if checkpoint.structured_events.is_empty() {
                    replay.structured_applied.push(StreamEvent::ToolCallStart {
                        call_id: tool.call_id.clone(),
                        name: tool.name.clone(),
                    });
                    if !tool.args.is_empty() {
                        replay
                            .structured_applied
                            .push(StreamEvent::ToolCallArgsDelta {
                                call_id: tool.call_id.clone(),
                                args_fragment: tool.args.clone(),
                            });
                    }
                }
            }
            let mut replay_messages = ReplyArenaWriter::new().with_standard_provider_json_views();
            let mut replay_reasoning = ReplyArenaWriter::new();
            for event in &mut checkpoint.structured_events {
                match event {
                    StreamEvent::TextDelta { text } => {
                        route_message_ranges.push_back(replay_messages.append_shared(text));
                    }
                    StreamEvent::ReasoningDelta { text } => {
                        let _ = replay_reasoning.append_shared(text);
                    }
                    StreamEvent::RefusalDelta { text } => {
                        replay.refusal_applied.push_str(text);
                    }
                    event if is_structured_replay_event(event) => {
                        replay.record_structured(event.clone());
                    }
                    _ => {}
                }
            }
            if !replay_messages.is_empty() {
                replay.message_applied = Some(replay_messages.seal());
            }
            if !replay_reasoning.is_empty() {
                replay.reasoning_applied = Some(replay_reasoning.seal());
            }
            replay.message.clone_from(&replay.message_applied);
            replay.reasoning.clone_from(&replay.reasoning_applied);
            replay.refusal.clone_from(&replay.refusal_applied);
            replay.structured_expected = normalized_structured_replay(&replay.structured_applied);
        }
        let mut deferred = Vec::<DeferredAccumulator>::new();
        if let Some(checkpoint) = checkpoint {
            tools.push(ToolAccumulator {
                item_id: checkpoint.tool_item_id.clone(),
                call_id: checkpoint.call_id.clone(),
                name: checkpoint.tool_name.clone(),
                args: checkpoint.args.clone(),
                requested_name: recovered_names.remove(&checkpoint.call_id),
                parsed_args: OnceLock::new(),
            });
            let tool_call = match provider_tool_block(&tools, &checkpoint.call_id) {
                Ok(tool_call) => tool_call,
                Err(error) => {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            };
            let resumed =
                if checkpoint.tool_name == "request_input" || checkpoint.tool_name == "plan" {
                    self.resume_request_input(&run_id, &mut tools, 0, &cancel, checkpoint.menu)
                        .await
                } else {
                    self.resume_tool_approval(&run_id, &mut tools, 0, &cancel, checkpoint)
                        .await
                };
            match resumed {
                Ok(result) => {
                    messages.push(Message::assistant(vec![tool_call]));
                    messages.push(result);
                }
                Err(DriveError::Cancelled) => {
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                Err(error) => {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            }
        }
        if let Some(checkpoint) = partial_stream {
            match self
                .resolve_partial_stream_recovery(&run_id, &cancel, &checkpoint.menu, true)
                .await
            {
                Ok(ErrorAction::ContinuePartial) => {
                    messages.push(Message::assistant(vec![Block::Text {
                        text: checkpoint.text,
                    }]));
                    messages.push(Message::user_text(
                        "The previous response was interrupted. Continue exactly where it stopped without repeating any completed text.",
                    ));
                }
                Ok(ErrorAction::RetryFresh) => {}
                Ok(_) => {
                    return self
                        .provider_failure_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            provider_protocol_error(
                                "recovered partial-stream menu resolved to an unsupported action",
                            ),
                        )
                        .await;
                }
                Err(DriveError::Cancelled) => {
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                Err(error) => {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            }
            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await {
                return self.errored_state_outcome(&run_id, error).await;
            }
        }
        if let Some(checkpoint) = child_wait {
            let mut assistant_blocks = Vec::with_capacity(checkpoint.tools.len());
            for checkpoint in checkpoint.tools {
                tools.push(ToolAccumulator {
                    item_id: checkpoint.tool_item_id,
                    call_id: checkpoint.call_id.clone(),
                    name: checkpoint.tool_name,
                    args: checkpoint.args,
                    requested_name: recovered_names.remove(&checkpoint.call_id),
                    parsed_args: OnceLock::new(),
                });
                match provider_tool_block(&tools, &checkpoint.call_id) {
                    Ok(block) => assistant_blocks.push(block),
                    Err(error) => {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                error,
                            )
                            .await;
                    }
                }
                deferred.push(DeferredAccumulator {
                    call_id: checkpoint.call_id,
                    ticket: checkpoint.ticket,
                    report_emitted: checkpoint.report_emitted,
                    child_result_emitted: checkpoint.child_result_emitted,
                    tool_result_emitted: checkpoint.tool_result_emitted,
                    item_completed: checkpoint.item_completed,
                });
            }
            if let Err(error) = self
                .commit_state(
                    &run_id,
                    RunState::Waiting {
                        reason: WaitReason::LocalChild,
                    },
                )
                .await
            {
                return self.errored_state_outcome(&run_id, error).await;
            }
            let results = match self
                .settle_deferred_tools(&run_id, &mut tools, &mut deferred, &cancel)
                .await
            {
                Ok(results) => results,
                Err(DriveError::Cancelled) => {
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                Err(error) => {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            };
            messages.push(Message::assistant(assistant_blocks));
            messages.extend(results);
        }
        // `Thinking` and the request-attempt marker describe one provider
        // dispatch boundary. Keep the state pending until the marker is ready
        // so the common path can journal both atomically. Any intervening
        // durable context/rotation boundary flushes the state first.
        let mut thinking_pending = true;
        let mut provider = Arc::clone(&self.provider);
        let mut usage_account = self.config.usage_account.clone();
        let mut rotation_budget_consumed = self.config.rotation_budget_consumed;
        let mut capability_fallback_consumed = false;
        if let Some(initial_rotation) = self.config.initial_rotation.take() {
            if let Err(error) = self
                .commit_pending_thinking(&run_id, &mut thinking_pending)
                .await
            {
                return self.errored_state_outcome(&run_id, error).await;
            }
            if let Err(error) = self
                .commit_payload(
                    &run_id,
                    EventPayload::Rotation(initial_rotation),
                    prompt_omit_render(),
                )
                .await
            {
                return self.errored_state_outcome(&run_id, error).await;
            }
            rotation_budget_consumed = true;
        }
        let resumed_logical_request_count = route_wait
            .as_ref()
            .and_then(|checkpoint| {
                usize::try_from(checkpoint.response_epoch.saturating_add(1)).ok()
            })
            .unwrap_or(0);
        let mut provider_request_count = self
            .config
            .provider_requests_already_made
            .max(resumed_logical_request_count);
        // Recovery reuses the durable warning, including its exact prompt text.
        // Only recovery pays for a journal scan; fresh turns start with no note.
        let mut soft_bound_emitted = false;
        if restore_budget {
            match self.restore_request_budget(&run_id).await {
                Ok((used, note)) => {
                    provider_request_count = provider_request_count.max(used);
                    if let Some(note) = note {
                        if !messages.contains(&Message::user_text(note.clone())) {
                            messages.push(Message::user_text(note));
                        }
                        soft_bound_emitted = true;
                    }
                }
                Err(error) => return self.errored_state_outcome(&run_id, error).await,
            }
        }
        let mut continuation_count = 0usize;
        let mut forced_compaction_used = false;
        // Once an ineffective compaction promotes this turn, later request
        // rounds may use the larger hard budget but must never compact again.
        let mut compaction_guard_consumed = false;
        // Durable automatic switch commands are distinct per successful hop,
        // even when a fallback chain revisits the same provider/model pair.
        let mut provider_pair_switch_ordinal = 0u32;
        // A route-wait recovery reissues the in-flight response as a physical
        // retry of the same logical request. Starting above zero preserves
        // the restored logical count and freezes wfcont's volatile provider
        // view; the durable physical attempt ordinal still advances below.
        let mut provider_attempt = usize::from(
            route_wait.is_some()
                || (self.config.recovery_request_local_usage && provider_request_count > 0),
        );
        // Freeze the cacheable boundary for every physical retry of one
        // logical provider request. Advancing or retreating this boundary on
        // a transport retry would change the wire body underneath an exact
        // replay and invalidate both the provider-view ledger and cache key.
        let mut logical_request_cacheable_history_end = stable_history_end;
        let mut completed_usage: Option<Usage> = None;
        let request_ordinals = self
            .config
            .provider_request_ordinals
            .clone()
            .unwrap_or_else(|| {
                ProviderRequestOrdinal::new(self.config.provider_request_ordinal_already_made)
            });
        let mut provider_request_ordinal;
        let mut previous_cache_request = self.config.cache_previous_request.clone();
        let mut previous_provider_view = self.config.cache_previous_provider_view.clone();
        let mut pending_previous_cache_request: Option<PreviousCacheRequest> = None;
        let mut previous_cache_request_sent_at: Option<tokio::time::Instant> = None;
        let mut cache_rewarm_pending = self.config.cache_initial_rewarm;
        if route_wait.is_some() {
            match self
                .wait_for_provider_route(&run_id, &cancel, &provider)
                .await
            {
                Ok(()) => thinking_pending = false,
                Err(DriveError::Cancelled) => {
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                Err(error) => {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            }
        }
        let mut volatile_context_epoch = None;
        // W-B: provider-executed tool rows and cited web sources are
        // TURN-scoped — a pause_turn boundary can split a server call from
        // its result across requests, and the bounded sources list journals
        // exactly once, under the finished message.
        let mut server_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        let mut web_sources: Vec<WebSource> = Vec::new();
        // These are one logical provider response, not one physical transport
        // attempt. A reconnect keeps them alive while replay filtering skips
        // already-applied structured events and tool effects.
        let route_has_message_events = !route_message_ranges.is_empty();
        let mut assistant_blocks = if route_has_message_events {
            Vec::new()
        } else {
            message.as_ref().map_or_else(Vec::new, |message| {
                vec![Block::Text {
                    text: message.snapshot(),
                }]
            })
        };
        let mut tool_results = Vec::new();
        let mut refusal_reason = replay.refusal.clone();
        if let Some(checkpoint) = route_wait.as_ref() {
            let mut completed_tools = checkpoint
                .completed_tools
                .iter()
                .map(|tool| (tool.call_id.clone(), tool))
                .collect::<HashMap<_, _>>();
            for event in &checkpoint.structured_events {
                match event {
                    StreamEvent::TextDelta { .. } if route_has_message_events => {
                        if let Some(text) = route_message_ranges.pop_front() {
                            append_assistant_text_block(&mut assistant_blocks, text);
                        }
                    }
                    StreamEvent::ProviderOpaque { provider, data } => {
                        assistant_blocks.push(Block::ProviderOpaque {
                            provider: provider.clone(),
                            data: data.clone(),
                        });
                    }
                    StreamEvent::ToolCallEnd { call_id } => {
                        if let Some(tool) = completed_tools.remove(call_id) {
                            let invalid =
                                tool.result.as_ref().is_some_and(invalid_tool_call_result);
                            assistant_blocks.push(Block::ToolCall {
                                call_id: tool.call_id.clone(),
                                name: tool.name.clone(),
                                args: if invalid {
                                    serde_json::json!({})
                                } else {
                                    tool.args.clone()
                                },
                            });
                            if let Some(result) = tool.result.as_ref() {
                                let projection = model_tool_result_projection(&tool.name, result);
                                tool_results.push(Message::tool_result_with_images(
                                    tool.call_id.clone(),
                                    projection.preview,
                                    projection.truncated,
                                    result.images.clone(),
                                ));
                            }
                        }
                    }
                    StreamEvent::ServerToolUse {
                        call_id,
                        name,
                        args,
                    } => {
                        server_calls.insert(call_id.clone(), (name.clone(), args.clone()));
                    }
                    StreamEvent::ServerToolResult { call_id, .. } => {
                        server_calls.remove(call_id);
                    }
                    StreamEvent::WebSources { sources } => {
                        for source in sources {
                            if web_sources.len() >= WEB_SOURCES_CAP {
                                break;
                            }
                            if !web_sources
                                .iter()
                                .any(|existing| existing.url == source.url)
                            {
                                web_sources.push(source.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        'requests: loop {
            if provider_attempt == 0 {
                let budget = self.request_budget();
                if !soft_bound_emitted && provider_request_count >= budget.tranche {
                    let status = self.request_budget_status(
                        &run_id,
                        provider_request_count,
                        RequestBudgetPhaseV1::SoftBound,
                    );
                    if let Err(error) = self.commit_request_budget_note(&run_id, &status).await {
                        return self.errored_state_outcome(&run_id, error).await;
                    }
                    messages.push(Message::user_text(status.model_note()));
                    soft_bound_emitted = true;
                }
                if provider_request_count >= budget.hard_cap {
                    if let Err(error) = self
                        .commit_pending_thinking(&run_id, &mut thinking_pending)
                        .await
                    {
                        return self.errored_state_outcome(&run_id, error).await;
                    }
                    let status = self.request_budget_status(
                        &run_id,
                        provider_request_count,
                        RequestBudgetPhaseV1::HardBound,
                    );
                    return self
                        .errored_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            request_budget_error(&status),
                        )
                        .await;
                }
            }
            if provider_attempt == 0 {
                if let Some(dispatcher) = self.dispatcher.as_ref() {
                    match dispatcher.refresh_volatile_context_tail().await {
                        Ok(Some(tail)) => volatile_user_tail = (!tail.is_empty()).then_some(tail),
                        Ok(None) => {}
                        Err(error) => {
                            if let Err(state_error) = self
                                .commit_pending_thinking(&run_id, &mut thinking_pending)
                                .await
                            {
                                return self.errored_state_outcome(&run_id, state_error).await;
                            }
                            return self
                                .errored_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                    }
                }
                // The run identity separates accepted turns; the tail digest
                // separates logical requests after durable workflow progress.
                // Physical retries keep this exact epoch and request body.
                volatile_context_epoch = volatile_user_tail
                    .as_ref()
                    .map(|tail| digest_json(&(run_id.clone(), tail)));
            }
            self.pending_nudges
                .extend(self.promoted_steers.drain_committed());
            let previous_cache_request_completed = pending_previous_cache_request.is_some();
            if let Some(completed) = pending_previous_cache_request.take() {
                previous_cache_request = Some(completed);
            }
            let newest_volatile_history_start = messages.len();
            messages.extend(
                std::mem::take(&mut self.pending_nudges)
                    .into_iter()
                    .map(Message::user_text),
            );
            let request_projection_compacted = match self
                .enforce_context_policy(
                    &run_id,
                    &mut messages,
                    &mut current_turn_start,
                    &mut latest_compaction_summary_end,
                    volatile_user_tail.as_deref(),
                    &mut provider,
                    &mut usage_account,
                    &mut stable_history_end,
                    &mut compaction_guard_consumed,
                    &mut provider_pair_switch_ordinal,
                    &mut thinking_pending,
                )
                .await
            {
                Ok(compacted) => compacted,
                Err(error) => {
                    if let Err(state_error) = self
                        .commit_pending_thinking(&run_id, &mut thinking_pending)
                        .await
                    {
                        return self.errored_state_outcome(&run_id, state_error).await;
                    }
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            error,
                        )
                        .await;
                }
            };
            if request_projection_compacted {
                cache_rewarm_pending = Some(CacheRewarmReasonV1::PlannedCompaction);
            }
            if provider_attempt == 0 {
                logical_request_cacheable_history_end =
                    if !request_projection_compacted && provider_request_count > 0 {
                        // A completed provider/tool round is immutable input for
                        // the next round. Advance the cache marker without
                        // changing the accepted current-user boundary used by
                        // compaction.
                        newest_volatile_history_start
                    } else {
                        stable_history_end
                    };
            }
            let cacheable_history_end = logical_request_cacheable_history_end;
            if provider_attempt == 0 {
                provider_request_count = provider_request_count.saturating_add(1);
            }
            provider_attempt = provider_attempt.saturating_add(1);
            provider_request_ordinal = match request_ordinals.next() {
                Ok(ordinal) => ordinal,
                Err(error) => {
                    return self
                        .errored_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            HaiderError::new(ErrorCode::Internal, error.to_string(), false),
                        )
                        .await;
                }
            };
            let request_correlation = ProviderRequestAttemptV1 {
                session_id: self.config.session_id.clone(),
                run_id: run_id.clone(),
                turn_ordinal: self.config.turn_ordinal,
                request_ordinal: provider_request_ordinal,
                request_kind: if self.config.agent_id.is_some() {
                    ProviderRequestKind::Side
                } else {
                    ProviderRequestKind::Primary
                },
            };
            if !request_correlation.coordinates_valid() {
                return self
                    .errored_outcome_with_items(
                        &run_id,
                        &mut message,
                        &mut reasoning,
                        &mut tools,
                        HaiderError::new(
                            ErrorCode::Internal,
                            "turn correlation coordinates are invalid or ambiguous",
                            false,
                        ),
                    )
                    .await;
            }
            if let Some(trace) = self.config.turn_trace.as_ref() {
                trace.register_request(&request_correlation);
            }
            let previous_stable_history_end = previous_provider_view
                .as_ref()
                .and_then(|view| usize::try_from(view.stable_history_end).ok())
                .or_else(|| {
                    previous_cache_request
                        .as_ref()
                        .map(|previous| previous.history_message_count)
                });
            // Move the canonical history into the provider request and take
            // it back immediately after the HTTP stream opens. Built-in
            // adapters borrow this request; only compatibility providers use
            // the trait's owned-clone fallback. Preserve the few tool-result
            // fields that request-only image budgeting may rewrite.
            let mut request_messages = std::mem::take(&mut messages);
            let (request_image_count, request_image_bytes) = request_messages
                .iter()
                .flat_map(|message| &message.blocks)
                .filter_map(|block| match block {
                    Block::ToolResult { images, .. } => Some(images),
                    _ => None,
                })
                .flatten()
                .fold((0_usize, 0_u64), |(count, bytes), image| {
                    (
                        count.saturating_add(1),
                        bytes.saturating_add(image.byte_len),
                    )
                });
            let request_images_will_mutate = !self.config.tool_result_images_supported
                || request_image_count > TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN
                || request_image_bytes > TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN;
            let mut request_only_tool_results =
                if request_images_will_mutate {
                    request_messages
                        .iter()
                        .enumerate()
                        .flat_map(|(message_index, message)| {
                            message.blocks.iter().enumerate().filter_map(
                                move |(block_index, block)| match block {
                                    Block::ToolResult {
                                        preview, images, ..
                                    } if !images.is_empty() => Some((
                                        message_index,
                                        block_index,
                                        preview.clone(),
                                        images.clone(),
                                    )),
                                    _ => None,
                                },
                            )
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
            let snapshot_insert_at = current_turn_start.min(request_messages.len());
            let mut request_stable_history_end = stable_history_end.min(request_messages.len());
            let mut request_cacheable_history_end =
                cacheable_history_end.min(request_messages.len());
            let mut request_current_user_start = snapshot_insert_at;
            if let Some(tail) = &volatile_user_tail {
                request_messages.insert(snapshot_insert_at, Message::user_text(tail.clone()));
                let snapshot_end = snapshot_insert_at.saturating_add(1);
                // The request snapshot is stable across physical retries. A
                // later provider/tool-loop boundary is expressed in durable-message
                // coordinates, so shift it past the inserted request block.
                request_stable_history_end = request_stable_history_end.max(snapshot_end);
                request_cacheable_history_end =
                    if request_cacheable_history_end > snapshot_insert_at {
                        request_cacheable_history_end.saturating_add(1)
                    } else {
                        snapshot_end
                    };
                request_current_user_start = snapshot_end;
            }
            // Cache-epoch construction needs the stable header components,
            // but built-in adapters replace the history digest from M4's
            // exact provider-view CAS. Defer normalized history hashing to
            // the compatibility fallback instead of serializing P twice.
            let mut prefix_digests = usage_prefix_digests(&self.config, &[]);
            prefix_digests.immutable_history.clear();
            let request_attachments =
                match self.resolve_tool_result_images(&mut request_messages).await {
                    Ok(attachments) => attachments,
                    Err(error) => {
                        if let Err(state_error) = self
                            .commit_pending_thinking(&run_id, &mut thinking_pending)
                            .await
                        {
                            return self.errored_state_outcome(&run_id, state_error).await;
                        }
                        return self
                            .errored_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                error,
                            )
                            .await;
                    }
                };
            // Cache metadata must exist before provider preparation, so this
            // is the last provider-neutral request-send boundary available to
            // the TTL selector. Sample every request at this same boundary;
            // usage processing may happen much later after an in-stream tool.
            let request_sent_at = tokio::time::Instant::now();
            if previous_cache_request_completed
                && let Some(previous_sent_at) = previous_cache_request_sent_at
            {
                let reuse_gap = request_sent_at.duration_since(previous_sent_at);
                self.config.cache_reuse_gap_ms =
                    Some(u64::try_from(reuse_gap.as_millis()).unwrap_or(u64::MAX));
            }
            let mut cache_metadata = prompt_cache_metadata(
                &self.config,
                &request_messages,
                PromptCacheBoundaries {
                    stable_history_end: request_stable_history_end,
                    cacheable_history_end: request_cacheable_history_end,
                    current_user_start: request_current_user_start,
                    previous_stable_history_end,
                    latest_compaction_summary_end,
                },
                prefix_digests.clone(),
                usage_account.as_ref(),
                volatile_context_epoch.as_deref(),
            );
            // Built-in adapters render directly from the daemon's Arc-backed
            // definitions. Standalone/injected providers retain the owned Vec
            // compatibility path below.
            let shared_request_tools = self.config.shared_tools.as_ref().map(Arc::clone);
            let request_tools = if shared_request_tools.is_some() {
                Vec::new()
            } else {
                std::mem::take(&mut self.config.tools)
            };
            let mut provider_request = TurnRequest {
                messages: request_messages,
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system_prompt: self.config.system_prompt.take(),
                tools: request_tools,
                attachments: request_attachments,
                cache_metadata: Some(cache_metadata.clone()),
            };
            let projected_input_tokens =
                estimate_if_budget_guarded(self.config.provider_budget_guard.as_deref(), || {
                    estimate_provider_request_input_tokens(
                        &provider_request.messages,
                        &provider_request.system_prompt,
                        shared_request_tools
                            .as_deref()
                            .map_or(provider_request.tools.as_slice(), |tools| tools),
                        &provider_request.attachments,
                    )
                });
            let mut prepared = if let Some(tools) = shared_request_tools.as_ref() {
                provider.prepare_turn_with_tools_owned(&mut provider_request, tools)
            } else {
                provider.prepare_turn_owned(&mut provider_request)
            };
            if let (Some(prepared), Some(trace)) =
                (prepared.as_mut(), self.config.turn_trace.as_ref())
            {
                prepared.set_turn_trace(trace.clone(), provider_request_ordinal);
            }
            let retained_wire = prepared
                .as_ref()
                .is_some_and(haider_provider::PreparedTurn::has_rendered_wire);
            self.config.system_prompt = provider_request.system_prompt.take();
            if shared_request_tools.is_none() {
                self.config.tools = std::mem::take(&mut provider_request.tools);
            }
            if retained_wire {
                // Anthropic's stream decoder needs only this capability bit;
                // the complete schemas already live in the retained wire.
                provider_request.tools.extend(
                    self.config
                        .tool_definitions()
                        .iter()
                        .filter(|tool| tool.name == "computer")
                        .cloned(),
                );
            } else {
                // Unknown/injected providers own their TurnRequest and retain
                // the compatibility clone behavior.
                provider_request
                    .system_prompt
                    .clone_from(&self.config.system_prompt);
                provider_request
                    .tools
                    .extend(self.config.tool_definitions().iter().cloned());
            }
            let previous_prefix_digests = if let Some(rendered) =
                prepared.as_ref().map(|prepared| prepared.prefix_digests())
            {
                prefix_digests = rendered.clone();
                prepared
                    .as_ref()
                    .and_then(|prepared| prepared.previous_immutable_history_digest())
                    .map(|history| {
                        let mut previous = rendered.clone();
                        previous.immutable_history = history.to_owned();
                        previous
                    })
            } else {
                // Legacy/injected providers report normalized diagnostics
                // over the canonical pre-projection history. Temporarily
                // swap back only fields changed by request image budgeting;
                // the outbound projection remains degraded afterward.
                for (message_index, block_index, preview, images) in &mut request_only_tool_results
                {
                    let request_message_index = message_index.saturating_add(usize::from(
                        volatile_user_tail.is_some() && *message_index >= snapshot_insert_at,
                    ));
                    if let Some(Block::ToolResult {
                        preview: request_preview,
                        images: request_images,
                        ..
                    }) = provider_request
                        .messages
                        .get_mut(request_message_index)
                        .and_then(|message| message.blocks.get_mut(*block_index))
                    {
                        std::mem::swap(request_preview, preview);
                        std::mem::swap(request_images, images);
                    }
                }
                let current_end =
                    request_cacheable_history_end.min(provider_request.messages.len());
                prefix_digests =
                    usage_prefix_digests(&self.config, &provider_request.messages[..current_end]);
                let previous_prefix_digests = previous_stable_history_end
                    .filter(|previous| *previous <= provider_request.messages.len())
                    .map(|previous| {
                        usage_prefix_digests(&self.config, &provider_request.messages[..previous])
                    });
                for (message_index, block_index, preview, images) in &mut request_only_tool_results
                {
                    let request_message_index = message_index.saturating_add(usize::from(
                        volatile_user_tail.is_some() && *message_index >= snapshot_insert_at,
                    ));
                    if let Some(Block::ToolResult {
                        preview: request_preview,
                        images: request_images,
                        ..
                    }) = provider_request
                        .messages
                        .get_mut(request_message_index)
                        .and_then(|message| message.blocks.get_mut(*block_index))
                    {
                        std::mem::swap(request_preview, preview);
                        std::mem::swap(request_images, images);
                    }
                }
                previous_prefix_digests
            };
            cache_metadata = prompt_cache_metadata(
                &self.config,
                &provider_request.messages,
                PromptCacheBoundaries {
                    stable_history_end: request_stable_history_end,
                    cacheable_history_end: request_cacheable_history_end,
                    current_user_start: request_current_user_start,
                    previous_stable_history_end,
                    latest_compaction_summary_end,
                },
                prefix_digests.clone(),
                usage_account.as_ref(),
                volatile_context_epoch.as_deref(),
            );
            provider_request.cache_metadata = Some(cache_metadata.clone());
            if let Some(provider_view) = prepared
                .as_ref()
                .and_then(|prepared| prepared.provider_view())
                && let Some(previous) = previous_provider_view.as_ref()
                && let Err(error) = validate_provider_view_prefix(previous, provider_view)
            {
                return self
                    .errored_outcome_with_items(
                        &run_id,
                        &mut message,
                        &mut reasoning,
                        &mut tools,
                        provider_view_invariant_error(error),
                    )
                    .await;
            }
            let mut pending_provider_view = None;
            let mut provider_view_attempt_data = None;
            if let Some(provider_view_ledger) = prepared
                .as_ref()
                .and_then(|prepared| prepared.provider_view())
                .map(|provider_view| provider_view.ledger().clone())
            {
                let blobs = prepared
                    .as_mut()
                    .map(haider_provider::PreparedTurn::take_provider_view_storage_blobs)
                    .unwrap_or_default();
                cache_metadata
                    .header_epoch
                    .clone_from(&provider_view_ledger.header_epoch);
                provider_request.cache_metadata = Some(cache_metadata.clone());
                let provider_view_attempt = ProviderViewAttemptV1 {
                    ordinal: provider_request_ordinal,
                    view: provider_view_ledger.clone(),
                };
                let provider_view_data = match serde_json::to_value(provider_view_attempt) {
                    Ok(data) => data,
                    Err(error) => {
                        return self
                            .errored_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                HaiderError::new(
                                    ErrorCode::Internal,
                                    format!("provider-view ledger could not serialize: {error}"),
                                    false,
                                ),
                            )
                            .await;
                    }
                };
                provider_view_attempt_data = Some(provider_view_data);
                pending_provider_view = Some((provider_view_ledger, blobs));
            }
            let cache_control = prepared
                .as_ref()
                .map_or(CacheControlObservationV1::Unavailable, |prepared| {
                    *prepared.cache_control()
                });
            let request_cache_diagnostic = build_cache_request_diagnostic(
                &self.config.cache_diagnostic_key,
                &cache_metadata.provider,
                &self.config.model,
                &cache_metadata.cache_epoch,
                &prefix_digests,
                previous_prefix_digests.as_ref(),
                previous_cache_request.as_ref(),
                request_cacheable_history_end,
                cache_metadata.stable_prefix_tokens,
                cache_metadata.reuse_gap_ms,
                cache_control,
                cache_rewarm_pending,
            );
            let request_attempt = CacheRequestAttemptV1 {
                ordinal: provider_request_ordinal,
                correlation: Some(request_correlation.clone()),
                diagnostic: request_cache_diagnostic.clone(),
            };
            let request_attempt_data = match serde_json::to_value(request_attempt) {
                Ok(data) => data,
                Err(error) => {
                    if let Err(state_error) = self
                        .commit_pending_thinking(&run_id, &mut thinking_pending)
                        .await
                    {
                        return self.errored_state_outcome(&run_id, state_error).await;
                    }
                    return self
                        .errored_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            HaiderError::new(
                                ErrorCode::Internal,
                                format!("cache request diagnostic could not serialize: {error}"),
                                false,
                            ),
                        )
                        .await;
                }
            };
            // P0 hard-budget boundary: the daemon sees the fully shaped
            // physical request before either its durable attempt marker or
            // provider transport can observe it. The returned permit keeps
            // capped parent/child requests serialized until this exchange
            // reaches a stream terminal.
            let mut provider_budget_permit = if let (Some(guard), Some(projected_input_tokens)) = (
                self.config.provider_budget_guard.as_ref(),
                projected_input_tokens,
            ) {
                match guard
                    .before_request(
                        &run_id,
                        &self.config.usage_scope.provider,
                        &provider_request,
                        projected_input_tokens,
                    )
                    .await
                {
                    Ok(permit) => Some(permit),
                    Err(error) => {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                DriveError::from(error),
                            )
                            .await;
                    }
                }
            } else {
                None
            };
            let request_attempt_commit_started = self
                .config
                .turn_trace
                .as_ref()
                .map(TurnTraceContext::now_us_from_accept);
            let persisted_provider_view = match self
                .commit_request_attempt(
                    &run_id,
                    provider_request_ordinal,
                    pending_provider_view,
                    RequestAttemptMarkers {
                        provider_view: provider_view_attempt_data,
                        cache: request_attempt_data,
                        response_epoch: replay.response_epoch,
                        request_budget: (provider_attempt == 1).then(|| {
                            self.request_budget_status(
                                &run_id,
                                provider_request_count,
                                RequestBudgetPhaseV1::Progress,
                            )
                        }),
                    },
                    &mut thinking_pending,
                )
                .await
            {
                Ok(stored) => stored,
                Err(error) => return self.errored_state_outcome(&run_id, error).await,
            };
            if let (Some(trace), Some(started)) =
                (&self.config.turn_trace, request_attempt_commit_started)
            {
                trace.emit(
                    "request_attempt_commit",
                    provider_request_ordinal,
                    0,
                    started,
                    trace.now_us_from_accept(),
                );
            }
            if let Some(stored_provider_view) = persisted_provider_view {
                previous_provider_view = Some(stored_provider_view);
            }
            // The durable attempt record above is the first post-compaction
            // request even if opening or streaming later fails. A retry gets
            // a fresh planned marker only when that failure itself triggers
            // another compaction.
            cache_rewarm_pending = None;
            let mut request_usage: Option<Usage> = None;
            let mut pending_usage_commit: Option<PendingUsageCommit> = None;
            let attempt_provider = Arc::clone(&provider);
            let provider_open_started = self
                .config
                .turn_trace
                .as_ref()
                .map(TurnTraceContext::now_us_from_accept);
            let trusts_default_route_absence = attempt_provider.trusts_default_route_absence();
            let opening_provider = Arc::clone(&attempt_provider);
            let provider_deadline_state = Arc::clone(&self.provider_deadline_state);
            let provider_request_ref = &provider_request;
            let request_metadata_body_support = opening_provider.request_metadata_body_support();
            let auxiliary_recorder = self.config.provider_request_attempt_recorder.clone();
            let opening_correlation = request_correlation.clone();
            let mut opening = Box::pin(before_provider_request_deadline(
                self.config.provider_deadline,
                async move {
                    // `before_provider_request_deadline` polls this future only
                    // after the next request has positive deadline admission.
                    // Until then, a prior retry remains in its admission state.
                    provider_deadline_state.begin_provider_request();
                    let open =
                        opening_provider.stream_prepared_turn_ref(provider_request_ref, prepared);
                    match auxiliary_recorder {
                        Some(recorder) => {
                            haider_provider::scope_provider_request_with_recorder(
                                opening_correlation,
                                request_metadata_body_support,
                                recorder,
                                open,
                            )
                            .await
                        }
                        None => {
                            haider_provider::scope_provider_request(
                                opening_correlation,
                                request_metadata_body_support,
                                open,
                            )
                            .await
                        }
                    }
                },
            ));
            let mut opening_network_waiting = false;
            let mut stream = loop {
                let opened = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        if let Err(error) = release_provider_budget_request(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            false,
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        return self
                            .cancelled_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                            )
                            .await;
                    }
                    () = tokio::time::sleep(ROUTE_STATE_POLL_INTERVAL) => {
                        let unavailable = trusts_default_route_absence
                            && attempt_provider.route_status()
                                == haider_platform::RouteStatus::Unavailable;
                        if unavailable != opening_network_waiting {
                            let state = if unavailable {
                                RunState::Waiting {
                                    reason: WaitReason::NetworkUnavailable,
                                }
                            } else {
                                RunState::Thinking
                            };
                            if let Err(error) = self.commit_state(&run_id, state).await {
                                let _ = release_provider_budget_request(
                                    self.config.provider_budget_guard.as_ref(),
                                    &run_id,
                                    &self.config.usage_scope.provider,
                                    &self.config.model,
                                    false,
                                    &mut provider_budget_permit,
                                )
                                .await;
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            opening_network_waiting = unavailable;
                        }
                        continue;
                    }
                    opened = &mut opening => opened,
                    command = self.commands.recv() => {
                        let Some(command) = command else {
                            if let Err(error) = release_provider_budget_request(
                                self.config.provider_budget_guard.as_ref(),
                                &run_id,
                                &self.config.usage_scope.provider,
                                &self.config.model,
                                false,
                                &mut provider_budget_permit,
                            )
                            .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            let error = provider_protocol_error(
                                "session actor command channel closed while opening provider stream",
                            );
                            return self
                                .provider_failure_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        };
                        if let ActorCommand::Stop { completed } = command {
                            cancel.cancel();
                            let outcome = match release_provider_budget_request(
                                self.config.provider_budget_guard.as_ref(),
                                &run_id,
                                &self.config.usage_scope.provider,
                                &self.config.model,
                                false,
                                &mut provider_budget_permit,
                            )
                            .await
                            {
                                Ok(()) => {
                                    self.cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await
                                }
                                Err(error) => {
                                    self.drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await
                                }
                            };
                            let _ = completed.send(());
                            return outcome;
                        }
                        self.service_command_without_menu(command);
                        continue;
                    }
                };
                if cancel.is_cancelled() {
                    if let Err(error) = release_provider_budget_request(
                        self.config.provider_budget_guard.as_ref(),
                        &run_id,
                        &self.config.usage_scope.provider,
                        &self.config.model,
                        false,
                        &mut provider_budget_permit,
                    )
                    .await
                    {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                DriveError::from(error),
                            )
                            .await;
                    }
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                drop(opening);
                let mut restored_messages = std::mem::take(&mut provider_request.messages);
                if volatile_user_tail.is_some() && snapshot_insert_at < restored_messages.len() {
                    restored_messages.remove(snapshot_insert_at);
                }
                for (message_index, block_index, preview, images) in
                    std::mem::take(&mut request_only_tool_results)
                {
                    if let Some(Block::ToolResult {
                        preview: restored_preview,
                        images: restored_images,
                        ..
                    }) = restored_messages
                        .get_mut(message_index)
                        .and_then(|message| message.blocks.get_mut(block_index))
                    {
                        *restored_preview = preview;
                        *restored_images = images;
                    }
                }
                messages = restored_messages;
                if opened.is_err()
                    && let Some(guard) = self.config.provider_budget_guard.clone()
                    && let Err(error) = guard
                        .after_request(
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            false,
                        )
                        .await
                {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            DriveError::from(error),
                        )
                        .await;
                }
                if opened.is_err() {
                    drop(provider_budget_permit.take());
                }
                match opened {
                    Ok(stream) => {
                        if let (Some(trace), Some(started)) =
                            (&self.config.turn_trace, provider_open_started)
                        {
                            trace.emit(
                                "provider_open",
                                provider_request_ordinal,
                                0,
                                started,
                                trace.now_us_from_accept(),
                            );
                        }
                        break stream;
                    }
                    Err(error) if error.kind == ProviderErrorKind::ContextExceeded => {
                        let compacted = if request_projection_compacted || compaction_guard_consumed
                        {
                            Err(repeated_context_overflow_after_compaction())
                        } else {
                            self.force_context_compaction(
                                &run_id,
                                &mut messages,
                                &mut stable_history_end,
                                &mut current_turn_start,
                                &mut latest_compaction_summary_end,
                                &mut forced_compaction_used,
                            )
                            .await
                        };
                        if let Err(error) = compacted {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                        cache_rewarm_pending = Some(CacheRewarmReasonV1::PlannedCompaction);
                        provider_attempt = 0;
                        replay.reset_for_next_request();
                        refusal_reason.clear();
                        assistant_blocks.clear();
                        tool_results.clear();
                        continue 'requests;
                    }
                    Err(error)
                        if provider_error_waits_for_route(&error, provider.route_status()) =>
                    {
                        if let Err(error) = release_provider_budget_for_route(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        replay.capture(&message, &reasoning, &refusal_reason);
                        match self
                            .wait_for_provider_route(&run_id, &cancel, &provider)
                            .await
                        {
                            Ok(()) => thinking_pending = false,
                            Err(DriveError::Cancelled) => {
                                return self
                                    .cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await;
                            }
                            Err(error) => {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                        }
                        continue 'requests;
                    }
                    Err(error) => {
                        if let Err(error) = self
                            .prepare_pre_first_event_retry(
                                ProviderRetryContext {
                                    run_id: &run_id,
                                    cancel: &cancel,
                                },
                                &mut provider_attempt,
                                &mut provider,
                                &mut usage_account,
                                &mut messages,
                                &mut stable_history_end,
                                &mut current_turn_start,
                                &mut latest_compaction_summary_end,
                                &mut rotation_budget_consumed,
                                &mut capability_fallback_consumed,
                                &mut provider_pair_switch_ordinal,
                                &mut thinking_pending,
                                error,
                            )
                            .await
                        {
                            return match error {
                                DriveError::Cancelled => {
                                    self.cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await
                                }
                                other => {
                                    self.drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        other,
                                    )
                                    .await
                                }
                            };
                        }
                        continue 'requests;
                    }
                }
            };
            if let Err(error) = self.commit_state(&run_id, RunState::Streaming).await {
                let _ = release_provider_budget_request(
                    self.config.provider_budget_guard.as_ref(),
                    &run_id,
                    &self.config.usage_scope.provider,
                    &self.config.model,
                    false,
                    &mut provider_budget_permit,
                )
                .await;
                return self.errored_state_outcome(&run_id, error).await;
            }

            let mut provider_content_seen = message.as_ref().is_some_and(|item| !item.is_empty())
                || reasoning.as_ref().is_some_and(|item| !item.is_empty())
                || replay.has_applied_content();
            let mut first_provider_event_seen = false;
            let provider_stream_started = self
                .config
                .turn_trace
                .as_ref()
                .map(TurnTraceContext::now_us_from_accept);
            loop {
                // Coalesce only a Finish already buffered immediately after
                // Usage. If polling would suspend, preserve the old durability
                // boundary by committing Usage before awaiting anything else.
                let immediate_next = (self.config.provider_budget_guard.is_none()
                    && pending_usage_commit.is_some()
                    && !cancel.is_cancelled())
                .then(|| poll_provider_stream_now(&mut stream))
                .flatten();
                if pending_usage_commit.is_some()
                    && immediate_next.is_none()
                    && let Err(error) = self
                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                        .await
                {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            DriveError::from(error),
                        )
                        .await;
                }
                let pending_delta_deadline = self.pending_item_delta_deadline;
                let next = if let Some(next) = immediate_next {
                    next
                } else {
                    tokio::select! {
                    // Cancellation owns ties. Provider progress is polled
                    // before command service on every round so an unbounded
                    // command arrival rate cannot starve the active stream.
                    biased;
                    () = cancel.cancelled() => {
                        if let Err(error) = release_provider_budget_request(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            request_usage.is_some(),
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        return self
                            .cancelled_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                            )
                            .await;
                    }
                    () = delta_flush_timer(pending_delta_deadline) => {
                        if let Err(error) = self.flush_pending_item_delta().await {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        continue;
                    }
                    item = stream.recv() => item,
                    command = self.commands.recv() => {
                        let Some(command) = command else {
                            if let Err(error) = release_provider_budget_request(
                                self.config.provider_budget_guard.as_ref(),
                                &run_id,
                                &self.config.usage_scope.provider,
                                &self.config.model,
                                request_usage.is_some(),
                                &mut provider_budget_permit,
                            )
                            .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            let error = provider_protocol_error(
                                "session actor command channel closed during provider stream",
                            );
                            return self
                                .provider_failure_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        };
                        if let ActorCommand::Stop { completed } = command {
                            cancel.cancel();
                            let outcome = match release_provider_budget_request(
                                self.config.provider_budget_guard.as_ref(),
                                &run_id,
                                &self.config.usage_scope.provider,
                                &self.config.model,
                                request_usage.is_some(),
                                &mut provider_budget_permit,
                            )
                            .await
                            {
                                Ok(()) => {
                                    self.cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await
                                }
                                Err(error) => {
                                    self.drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await
                                }
                            };
                            let _ = completed.send(());
                            return outcome;
                        }
                        self.service_command_without_menu(command);
                        continue;
                    }
                    }
                };

                let finish_follows_usage = matches!(&next, Some(Ok(StreamEvent::Finish { .. })));
                if !finish_follows_usage
                    && let Err(error) = self
                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                        .await
                {
                    return self
                        .drive_error_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                            DriveError::from(error),
                        )
                        .await;
                }

                if cancel.is_cancelled() {
                    if let Err(error) = self
                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                        .await
                    {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                DriveError::from(error),
                            )
                            .await;
                    }
                    if let Err(error) = release_provider_budget_request(
                        self.config.provider_budget_guard.as_ref(),
                        &run_id,
                        &self.config.usage_scope.provider,
                        &self.config.model,
                        request_usage.is_some(),
                        &mut provider_budget_permit,
                    )
                    .await
                    {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                DriveError::from(error),
                            )
                            .await;
                    }
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                let next = next.unwrap_or_else(|| {
                    Err(provider_stream_interrupted(
                        "provider stream closed before a finish event",
                    ))
                });
                let event = match next {
                    Ok(event) => {
                        if !first_provider_event_seen {
                            first_provider_event_seen = true;
                            if let (Some(trace), Some(started)) =
                                (&self.config.turn_trace, provider_stream_started)
                            {
                                let _ = started;
                                trace.emit_first_byte(provider_request_ordinal);
                            }
                        }
                        if !matches!(
                            event,
                            StreamEvent::UsageUpdate(_)
                                | StreamEvent::NetworkUnavailable
                                | StreamEvent::NetworkRestored
                        ) {
                            provider_content_seen = true;
                        }
                        event
                    }
                    Err(error)
                        if error.kind == ProviderErrorKind::ContextExceeded
                            && !provider_content_seen =>
                    {
                        if let Err(error) = release_provider_budget_request(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            request_usage.is_some(),
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        let compacted = if request_projection_compacted || compaction_guard_consumed
                        {
                            Err(repeated_context_overflow_after_compaction())
                        } else {
                            self.force_context_compaction(
                                &run_id,
                                &mut messages,
                                &mut stable_history_end,
                                &mut current_turn_start,
                                &mut latest_compaction_summary_end,
                                &mut forced_compaction_used,
                            )
                            .await
                        };
                        if let Err(error) = compacted {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                        cache_rewarm_pending = Some(CacheRewarmReasonV1::PlannedCompaction);
                        provider_attempt = 0;
                        replay.reset_for_next_request();
                        refusal_reason.clear();
                        assistant_blocks.clear();
                        tool_results.clear();
                        continue 'requests;
                    }
                    Err(error)
                        if provider_error_waits_for_route(&error, provider.route_status()) =>
                    {
                        if let Err(error) = self
                            .commit_pending_usage(&run_id, &mut pending_usage_commit)
                            .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        if let Err(error) = self.flush_pending_item_delta().await {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        if let Err(error) = release_provider_budget_for_route(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        replay.capture(&message, &reasoning, &refusal_reason);
                        match self
                            .wait_for_provider_route(&run_id, &cancel, &provider)
                            .await
                        {
                            Ok(()) => thinking_pending = false,
                            Err(DriveError::Cancelled) => {
                                return self
                                    .cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await;
                            }
                            Err(error) => {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                        }
                        continue 'requests;
                    }
                    Err(error) if !provider_content_seen => {
                        if let Err(error) = release_provider_budget_request(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            request_usage.is_some(),
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        if let Err(error) = self
                            .prepare_pre_first_event_retry(
                                ProviderRetryContext {
                                    run_id: &run_id,
                                    cancel: &cancel,
                                },
                                &mut provider_attempt,
                                &mut provider,
                                &mut usage_account,
                                &mut messages,
                                &mut stable_history_end,
                                &mut current_turn_start,
                                &mut latest_compaction_summary_end,
                                &mut rotation_budget_consumed,
                                &mut capability_fallback_consumed,
                                &mut provider_pair_switch_ordinal,
                                &mut thinking_pending,
                                error,
                            )
                            .await
                        {
                            return match error {
                                DriveError::Cancelled => {
                                    self.cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await
                                }
                                other => {
                                    self.drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        other,
                                    )
                                    .await
                                }
                            };
                        }
                        continue 'requests;
                    }
                    Err(error) => {
                        if let Err(budget_error) = release_provider_budget_request(
                            self.config.provider_budget_guard.as_ref(),
                            &run_id,
                            &self.config.usage_scope.provider,
                            &self.config.model,
                            request_usage.is_some(),
                            &mut provider_budget_permit,
                        )
                        .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(budget_error),
                                )
                                .await;
                        }
                        // A network disconnect or idle timeout is not a user
                        // decision. Once provider content is visible we cannot
                        // safely replay the request, so terminate through the
                        // typed RunFailed/Errored path instead of opening a
                        // partial-stream input menu. Non-transport response
                        // failures retain the explicit recovery menu below.
                        if provider_error_is_transport_fault(&error) {
                            return self
                                .provider_failure_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                        if message.as_ref().is_some_and(|partial| !partial.is_empty()) {
                            let presentation = stream_interruption_presentation(&error);
                            let (source_item, partial) = match self
                                .complete_incomplete_message(
                                    &run_id,
                                    &mut message,
                                    presentation.clone(),
                                )
                                .await
                            {
                                Ok(completed) => completed,
                                Err(error) => {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                            };
                            if let Err(error) =
                                self.complete_text(&run_id, &mut reasoning, true).await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            if let Err(error) = self
                                .complete_all_tools(&run_id, &mut tools, ToolStatus::Failed)
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            let autonomous_partial = self
                                .config
                                .interaction_policy
                                .resolve(InteractionGate::PartialProviderStream)
                                == InteractionResolution::ContinuePartial;
                            let menu = recovery_menu(
                                self.next_menu_id(),
                                &run_id,
                                Some(source_item),
                                ErrorRecoveryCardKind::PartialStream,
                                presentation,
                                Some(self.config.usage_scope.provider.clone()),
                                usage_account.clone(),
                                !autonomous_partial,
                            );
                            if let Err(error) = self
                                .commit_payload(
                                    &run_id,
                                    EventPayload::MenuOpened(menu.clone()),
                                    prompt_omit_render(),
                                )
                                .await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            let action = match self
                                .resolve_partial_stream_recovery(&run_id, &cancel, &menu, false)
                                .await
                            {
                                Ok(action) => action,
                                Err(DriveError::Cancelled) => {
                                    return self
                                        .cancelled_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                        )
                                        .await;
                                }
                                Err(error) => {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                            };
                            match action {
                                ErrorAction::ContinuePartial => {
                                    messages.push(Message::assistant(vec![Block::Text {
                                        text: partial,
                                    }]));
                                    messages.push(Message::user_text(
                                        "The previous response was interrupted. Continue exactly where it stopped without repeating any completed text.",
                                    ));
                                }
                                ErrorAction::RetryFresh => {}
                                _ => {
                                    let error = provider_protocol_error(
                                        "partial-stream menu resolved to an unsupported action",
                                    );
                                    return self
                                        .provider_failure_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                            }
                            provider_attempt = 0;
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            tool_results.clear();
                            continue 'requests;
                        }
                        return self
                            .provider_failure_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                error,
                            )
                            .await;
                    }
                };

                match &event {
                    StreamEvent::NetworkUnavailable
                        if provider.route_status() == haider_platform::RouteStatus::Unavailable =>
                    {
                        if let Err(error) = self
                            .commit_state(
                                &run_id,
                                RunState::Waiting {
                                    reason: WaitReason::NetworkUnavailable,
                                },
                            )
                            .await
                        {
                            return self.errored_state_outcome(&run_id, error).await;
                        }
                        continue;
                    }
                    StreamEvent::NetworkUnavailable => continue,
                    StreamEvent::NetworkRestored
                        if provider.route_status() != haider_platform::RouteStatus::Unavailable =>
                    {
                        if let Err(error) = self.commit_state(&run_id, RunState::Streaming).await {
                            return self.errored_state_outcome(&run_id, error).await;
                        }
                        continue;
                    }
                    StreamEvent::NetworkRestored => continue,
                    _ => {}
                }

                // `stream.recv()` is intentionally biased ahead of command
                // service, so drain already-arrived input once more at the
                // exact pre-dispatch boundary. Otherwise a ready ToolCallEnd
                // could win the tie and execute before a Subturn command that
                // was already accepted into the actor queue.
                if matches!(&event, StreamEvent::ToolCallEnd { .. }) {
                    loop {
                        match self.commands.try_recv() {
                            Ok(ActorCommand::Stop { completed }) => {
                                cancel.cancel();
                                let outcome = match release_provider_budget_request(
                                    self.config.provider_budget_guard.as_ref(),
                                    &run_id,
                                    &self.config.usage_scope.provider,
                                    &self.config.model,
                                    request_usage.is_some(),
                                    &mut provider_budget_permit,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        self.cancelled_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                        )
                                        .await
                                    }
                                    Err(error) => {
                                        self.drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            DriveError::from(error),
                                        )
                                        .await
                                    }
                                };
                                let _ = completed.send(());
                                return outcome;
                            }
                            Ok(command) => self.service_command_without_menu(command),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                }
                let event = match replay.filter_structured(event) {
                    Ok(Some(event)) => event,
                    Ok(None) => continue,
                    Err(error) => {
                        return self
                            .provider_failure_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                error,
                            )
                            .await;
                    }
                };
                let mut route_replay_event =
                    is_structured_replay_event(&event).then(|| event.clone());

                let event_result: Result<Option<Message>, DriveError> = match event {
                    // Consumed by the state-transition gate immediately above.
                    StreamEvent::NetworkUnavailable | StreamEvent::NetworkRestored => Ok(None),
                    StreamEvent::TextDelta { text } => {
                        let text = replay.filter_message(text);
                        if text.is_empty() {
                            Ok(None)
                        } else {
                            match self
                                .apply_text_delta(&run_id, &mut message, text, false)
                                .await
                            {
                                Ok(range) => {
                                    append_assistant_text_block(&mut assistant_blocks, range);
                                    replay.message_applied =
                                        message.as_ref().map(TextAccumulator::snapshot);
                                    Ok(None)
                                }
                                Err(error) => Err(error),
                            }
                        }
                    }
                    StreamEvent::ReasoningDelta { text } => {
                        // Normalized reasoning has no provider-valid signature
                        // and must never be replayed into a follow-up request.
                        let text = replay.filter_reasoning(text);
                        if text.is_empty() {
                            Ok(None)
                        } else {
                            match self
                                .apply_text_delta(&run_id, &mut reasoning, text, true)
                                .await
                            {
                                Ok(_) => {
                                    replay.reasoning_applied =
                                        reasoning.as_ref().map(TextAccumulator::snapshot);
                                    Ok(None)
                                }
                                Err(error) => Err(error),
                            }
                        }
                    }
                    StreamEvent::RefusalDelta { text } => {
                        // Refusal content has its own provider channel. The
                        // terminal Refusal outcome survives, but this content
                        // must never become assistant text or prompt history.
                        let text = replay.filter_refusal(text);
                        append_bounded_refusal(&mut refusal_reason, &text);
                        if !text.is_empty() {
                            route_replay_event = Some(StreamEvent::RefusalDelta { text });
                        }
                        Ok(None)
                    }
                    StreamEvent::ProviderOpaque { provider, data } => {
                        async {
                            self.complete_text(&run_id, &mut message, false).await?;
                            self.complete_text(&run_id, &mut reasoning, true).await?;
                            let block = Block::ProviderOpaque { provider, data };
                            self.commit_provider_opaque(&run_id, &block).await?;
                            assistant_blocks.push(block);
                            Ok(None)
                        }
                        .await
                    }
                    StreamEvent::ToolCallStart { call_id, name } => {
                        async {
                            self.complete_text(&run_id, &mut message, false).await?;
                            self.complete_text(&run_id, &mut reasoning, true).await?;
                            self.start_tool(&run_id, &mut tools, call_id, name).await?;
                            Ok(None)
                        }
                        .await
                    }
                    StreamEvent::ToolCallArgsDelta {
                        call_id,
                        args_fragment,
                    } => self
                        .apply_tool_delta(&run_id, &mut tools, &call_id, args_fragment)
                        .await
                        .map(|()| None),
                    StreamEvent::ToolCallEnd { call_id } => {
                        match provider_tool_block(&tools, &call_id) {
                            Ok(block) => {
                                // Persist the reset before dispatch, including deferred tools.
                                // Their results may arrive after a later malformed frame, so
                                // result-completion order cannot reconstruct frame validity.
                                if malformed_tool_pending_repair {
                                    if let Err(error) = self
                                        .commit_hidden_extension_marker(
                                            &run_id,
                                            TOOL_CALL_REPAIR_RESET_EXTENSION_KIND,
                                            serde_json::json!({ "call_id": call_id }),
                                        )
                                        .await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                DriveError::Store(error),
                                            )
                                            .await;
                                    }
                                    malformed_tool_pending_repair = false;
                                }
                                assistant_blocks.push(block);
                                if !self.pending_subturns.is_empty() {
                                    if let Err(error) =
                                        self.complete_tools_for_subturn(&run_id, &mut tools).await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                error,
                                            )
                                            .await;
                                    }
                                    if !assistant_blocks.is_empty() {
                                        messages.push(Message::assistant(std::mem::take(
                                            &mut assistant_blocks,
                                        )));
                                    }
                                    // Close the provider protocol's pending
                                    // tool-use pair without claiming it ran.
                                    // The following user messages then form
                                    // the actual subturn request.
                                    messages.push(Message::tool_result(
                                        call_id,
                                        "held before execution for a user subturn; revise or confirm the tool call",
                                        false,
                                    ));
                                    messages.extend(
                                        std::mem::take(&mut self.pending_subturns)
                                            .into_iter()
                                            .map(Message::user_text),
                                    );
                                    if let Err(error) = release_provider_budget_request(
                                        self.config.provider_budget_guard.as_ref(),
                                        &run_id,
                                        &self.config.usage_scope.provider,
                                        &self.config.model,
                                        request_usage.is_some(),
                                        &mut provider_budget_permit,
                                    )
                                    .await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                DriveError::from(error),
                                            )
                                            .await;
                                    }
                                    if let Err(error) = finalize_request_usage(
                                        &mut completed_usage,
                                        &mut request_usage,
                                    ) {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                error,
                                            )
                                            .await;
                                    }
                                    provider_attempt = 0;
                                    thinking_pending = true;
                                    replay.reset_for_next_request();
                                    refusal_reason.clear();
                                    assistant_blocks.clear();
                                    tool_results.clear();
                                    continue 'requests;
                                }
                                self.complete_tool(
                                    &run_id,
                                    &mut tools,
                                    &mut deferred,
                                    &call_id,
                                    &cancel,
                                )
                                .await
                            }
                            Err(DriveError::Provider(error))
                                if error.presentation.subcode.as_str()
                                    == "malformed-tool-arguments" =>
                            {
                                let (block, result) = match self
                                    .close_malformed_tool_failure(
                                        &run_id, &mut tools, &call_id, &error,
                                    )
                                    .await
                                {
                                    Ok(pair) => pair,
                                    Err(close_error) => {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                close_error,
                                            )
                                            .await;
                                    }
                                };
                                if malformed_tool_pending_repair {
                                    return self
                                        .provider_failure_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                                malformed_tool_pending_repair = true;
                                assistant_blocks.push(block);
                                Ok(Some(result))
                            }
                            Err(error) => Err(error),
                        }
                    }
                    // W-B: a PROVIDER-executed tool call never enters the
                    // local dispatch loop; the args are held until its result
                    // lands so the row commits as one closed pair.
                    StreamEvent::ServerToolUse {
                        call_id,
                        name,
                        args,
                    } => {
                        if let Err(error) = self.flush_pending_item_delta().await {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        server_calls.insert(call_id, (name, args));
                        Ok(None)
                    }
                    StreamEvent::ServerToolResult {
                        call_id,
                        preview,
                        is_error,
                    } => {
                        let (name, args) = server_calls
                            .remove(&call_id)
                            .unwrap_or_else(|| ("web_tool".into(), serde_json::Value::Null));
                        async {
                            self.complete_text(&run_id, &mut message, false).await?;
                            self.complete_text(&run_id, &mut reasoning, true).await?;
                            let status = if is_error {
                                ToolStatus::Failed
                            } else {
                                ToolStatus::Completed
                            };
                            let result = BoundedResult {
                                preview,
                                truncated: false,
                                data: None,
                                artifact: None,
                                images: Vec::new(),
                                cursor: None,
                                status: if is_error {
                                    ToolResultStatus::Failed
                                } else {
                                    ToolResultStatus::Completed
                                },
                                reason: is_error.then(|| "server tool reported an error".into()),
                                presentation: is_error.then(|| {
                                    tool_error_presentation(
                                        "server-tool-failed",
                                        "Provider tool failed",
                                        "The provider-hosted tool reported an error.",
                                    )
                                }),
                            };
                            self.commit_server_tool_row(
                                &run_id, &call_id, name, args, status, &result,
                            )
                            .await?;
                            Ok(None)
                        }
                        .await
                    }
                    StreamEvent::WebSources { sources } => {
                        for source in sources {
                            if web_sources.len() >= WEB_SOURCES_CAP {
                                break;
                            }
                            if !web_sources
                                .iter()
                                .any(|existing| existing.url == source.url)
                            {
                                web_sources.push(source);
                            }
                        }
                        Ok(None)
                    }
                    StreamEvent::UsageUpdate(mut usage) => {
                        attach_usage_scope_and_cost(
                            &self.config,
                            &run_id,
                            &cache_metadata.cache_epoch,
                            cache_metadata.stable_prefix_tokens,
                            &mut usage,
                        );
                        if let Some(account) = &usage_account {
                            usage.account = Some(account.clone());
                            if let Some(scope) = &mut usage.scope {
                                scope.account_scope = Some(account.clone());
                            }
                            usage.accounts = vec![AccountUsage {
                                account: account.clone(),
                                input: usage.input,
                                output: usage.output,
                                reasoning: usage.reasoning,
                                cached: usage.cached,
                                source: usage.source,
                                normalized: usage.normalized.clone(),
                                scope: usage.scope.clone(),
                                cache_cost: usage.cache_cost,
                            }];
                        }
                        let mut cache_diagnostic = request_cache_diagnostic.clone();
                        cache_diagnostic.classification =
                            classify_cache_request(&cache_diagnostic, usage.normalized.as_ref());
                        usage.request = Some(RequestUsage {
                            ordinal: provider_request_ordinal,
                            input: usage.input,
                            output: usage.output,
                            reasoning: (usage.reasoning > 0).then_some(usage.reasoning),
                            cached: (usage.cached > 0
                                || usage.normalized.as_ref().is_some_and(|normalized| {
                                    normalized.cache_status == CacheStatAvailability::Present
                                }))
                            .then_some(usage.cached),
                            source: usage.source,
                            account: usage.account.clone(),
                            normalized: usage.normalized.clone(),
                            cache_cost: usage.cache_cost,
                            cache: Some(cache_diagnostic.clone()),
                        });
                        pending_previous_cache_request = Some(PreviousCacheRequest {
                            history_message_count: request_cacheable_history_end,
                            breakpoint_hashes: cache_diagnostic.breakpoint_hashes,
                            cache_domain_hash: cache_diagnostic.cache_domain_hash,
                        });
                        previous_cache_request_sent_at = Some(request_sent_at);
                        let footprint =
                            context_footprint_from_usage(&self.config, &usage, &messages);
                        request_usage = Some(usage.clone());
                        let durable_usage = if self.config.recovery_request_local_usage {
                            Ok(usage.clone())
                        } else {
                            cumulative_usage(completed_usage.as_ref(), &usage)
                        };
                        match durable_usage {
                            Ok(usage) => {
                                if let Err(error) = self.flush_pending_item_delta().await {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            DriveError::from(error),
                                        )
                                        .await;
                                }
                                pending_usage_commit = Some(PendingUsageCommit {
                                    footprint: self
                                        .config
                                        .context_compaction_v1
                                        .then_some(footprint),
                                    usage,
                                });
                                if let Some(guard) = self.config.provider_budget_guard.clone() {
                                    if let Err(error) = self
                                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                        .await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                DriveError::from(error),
                                            )
                                            .await;
                                    }
                                    if let Err(error) = guard.after_usage(&run_id).await {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                DriveError::from(error),
                                            )
                                            .await;
                                    }
                                }
                                Ok(None)
                            }
                            Err(error) => Err(error),
                        }
                    }
                    StreamEvent::Finish { reason } => {
                        if let (Some(trace), Some(started)) =
                            (&self.config.turn_trace, provider_stream_started)
                        {
                            trace.emit(
                                "provider_stream",
                                provider_request_ordinal,
                                0,
                                started,
                                trace.now_us_from_accept(),
                            );
                        }
                        if let Some(guard) = self.config.provider_budget_guard.clone()
                            && let Err(error) = guard
                                .after_request(
                                    &run_id,
                                    &self.config.usage_scope.provider,
                                    &self.config.model,
                                    request_usage.is_some(),
                                )
                                .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    DriveError::from(error),
                                )
                                .await;
                        }
                        drop(provider_budget_permit.take());
                        if reason == FinishReason::Cancelled {
                            if let Err(error) = self
                                .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            return self
                                .cancelled_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                )
                                .await;
                        }
                        if reason == FinishReason::Error {
                            if let Err(error) = self
                                .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            let error = HaiderError::new(
                                ErrorCode::ProviderError,
                                "provider finished the turn with an error",
                                false,
                            );
                            return self
                                .errored_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                        let mut post_stream_batch = reason == FinishReason::EndTurn
                            && pending_usage_commit.is_some()
                            && message.is_some()
                            && self.tree_head_initialized
                            && !replay.crossed_tool_boundary()
                            && tools.is_empty()
                            && deferred.is_empty()
                            && server_calls.is_empty()
                            && self.pending_subturns.is_empty()
                            && self.pending_nudges.is_empty()
                            && web_sources.is_empty()
                            && self.config.finalization_guard.is_none();
                        if !post_stream_batch {
                            if let Err(error) = self
                                .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            if let Err(error) =
                                self.complete_text(&run_id, &mut message, false).await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            if let Err(error) =
                                self.complete_text(&run_id, &mut reasoning, true).await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                        }
                        if !post_stream_batch
                            && let Err(error) = self
                                .complete_non_deferred_tools(
                                    &run_id,
                                    &mut tools,
                                    &deferred,
                                    ToolStatus::Pending,
                                )
                                .await
                        {
                            return self
                                .drive_error_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                    error,
                                )
                                .await;
                        }
                        if cancel.is_cancelled() {
                            if let Err(error) = self
                                .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await;
                            }
                            return self
                                .cancelled_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                )
                                .await;
                        }
                        if reason == FinishReason::Refusal {
                            let reason = normalized_refusal_reason(&refusal_reason);
                            if let Err(error) = self
                                .commit_closed_item_omitted(&run_id, TurnItem::Refusal { reason })
                                .await
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                        }
                        let current_assistant_message_index = if !assistant_blocks.is_empty() {
                            let index = messages.len();
                            messages
                                .push(Message::assistant(std::mem::take(&mut assistant_blocks)));
                            Some(index)
                        } else {
                            None
                        };
                        if !deferred.is_empty() {
                            if let Err(error) = self
                                .commit_state(
                                    &run_id,
                                    RunState::Waiting {
                                        reason: WaitReason::LocalChild,
                                    },
                                )
                                .await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            match self
                                .settle_deferred_tools(&run_id, &mut tools, &mut deferred, &cancel)
                                .await
                            {
                                Ok(mut results) => tool_results.append(&mut results),
                                Err(DriveError::Cancelled) => {
                                    return self
                                        .cancelled_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                        )
                                        .await;
                                }
                                Err(error) => {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                            }
                        }
                        if !tool_results.is_empty() {
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            provider_attempt = 0;
                            messages.append(&mut tool_results);
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            continue 'requests;
                        }
                        // W-B (LW2): `pause_turn` shares the MaxTokens
                        // continuation machinery (checkpoint + cap), but the
                        // paused assistant message is resent UNCHANGED — no
                        // synthesized user nudge joins the conversation.
                        if reason == FinishReason::MaxTokens || reason == FinishReason::PauseTurn {
                            continuation_count = continuation_count.saturating_add(1);
                            if continuation_count > self.config.max_continuations_per_turn {
                                return self
                                    .errored_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        continuation_limit_error(
                                            continuation_count,
                                            self.config.max_continuations_per_turn,
                                        ),
                                    )
                                    .await;
                            }
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            let request_index =
                                u32::try_from(provider_request_count).unwrap_or(u32::MAX);
                            let checkpoint = match serde_json::to_value(ContinuationCheckpoint {
                                reason,
                                request_index,
                            }) {
                                Ok(checkpoint) => checkpoint,
                                Err(error) => {
                                    return self
                                        .errored_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            HaiderError::new(
                                                ErrorCode::Internal,
                                                format!(
                                                    "continuation checkpoint could not serialize: {error}"
                                                ),
                                                false,
                                            ),
                                        )
                                        .await;
                                }
                            };
                            if let Err(error) = self
                                .commit_hidden_extension_marker(
                                    &run_id,
                                    CONTINUATION_CHECKPOINT_EXTENSION_KIND,
                                    checkpoint,
                                )
                                .await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            if reason == FinishReason::MaxTokens {
                                messages.push(Message::user_text(
                                    "Continue exactly where you stopped. Do not repeat completed content.",
                                ));
                            }
                            provider_attempt = 0;
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            tool_results.clear();
                            continue 'requests;
                        }
                        // No tool-call boundary appeared in this response.
                        // Deliver Subturn input at the completed-response
                        // boundary, matching Queue's end-of-turn timing while
                        // retaining the active run's durable receipt.
                        if !self.pending_subturns.is_empty() {
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            messages.extend(
                                std::mem::take(&mut self.pending_subturns)
                                    .into_iter()
                                    .map(Message::user_text),
                            );
                            provider_attempt = 0;
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            tool_results.clear();
                            continue 'requests;
                        }
                        if !self.pending_nudges.is_empty() {
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            provider_attempt = 0;
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            tool_results.clear();
                            continue 'requests;
                        }
                        // W-B (decision 6): the bounded sources list journals
                        // once, under the finished message — UI-visible,
                        // prompt-omitted (replay rides the opaque channel).
                        if !web_sources.is_empty() {
                            let sources = std::mem::take(&mut web_sources);
                            let data = serde_json::json!({ "sources": sources });
                            if let Err(error) = self
                                .commit_ui_extension_marker(
                                    &run_id,
                                    WEB_SOURCES_EXTENSION_KIND,
                                    data,
                                )
                                .await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                        }
                        if reason == FinishReason::EndTurn
                            && let Some(guard) = self.config.finalization_guard.clone()
                        {
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            loop {
                                let decision = match guard
                                    .before_done_after_requests(&run_id, provider_request_count)
                                    .await
                                {
                                    Ok(decision) => decision,
                                    Err(error) => {
                                        return self
                                            .errored_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                error,
                                            )
                                            .await;
                                    }
                                };
                                match decision {
                                    FinalizationGuardDecision::AllowDone => break,
                                    FinalizationGuardDecision::Continue { reminder } => {
                                        if let Some(reminder) = reminder {
                                            messages.push(Message::user_text(reminder));
                                        }
                                        provider_attempt = 0;
                                        thinking_pending = true;
                                        replay.reset_for_next_request();
                                        refusal_reason.clear();
                                        assistant_blocks.clear();
                                        tool_results.clear();
                                        continue 'requests;
                                    }
                                    FinalizationGuardDecision::ConfirmRequired(menu) => {
                                        if let Err(error) = self
                                            .commit_state(
                                                &run_id,
                                                RunState::InputRequired {
                                                    menu: menu.id.clone(),
                                                },
                                            )
                                            .await
                                        {
                                            return self
                                                .errored_state_outcome(&run_id, error)
                                                .await;
                                        }
                                        match self
                                            .wait_for_graph_finalization_answer(
                                                &run_id, &cancel, &menu,
                                            )
                                            .await
                                        {
                                            Ok(GraphFinalizationAnswer::ContinueWork) => {
                                                messages.push(Message::user_text(
                                                    "Continue working on the current workflow obligations. Do not finalize until they are satisfied, or ask for explicit abandonment again.",
                                                ));
                                                provider_attempt = 0;
                                                thinking_pending = true;
                                                replay.reset_for_next_request();
                                                refusal_reason.clear();
                                                assistant_blocks.clear();
                                                tool_results.clear();
                                                continue 'requests;
                                            }
                                            Ok(GraphFinalizationAnswer::AbandonAndFinish) => {
                                                // Re-consult durable authority: a graph pin/switch
                                                // may race the menu settlement before Done.
                                            }
                                            Ok(GraphFinalizationAnswer::Reconsult) => {
                                                // A concurrent graph event closed the durable card.
                                                // Re-read authority instead of parking forever.
                                            }
                                            Err(DriveError::Cancelled) => {
                                                return self
                                                    .cancelled_outcome_with_items(
                                                        &run_id,
                                                        &mut message,
                                                        &mut reasoning,
                                                        &mut tools,
                                                    )
                                                    .await;
                                            }
                                            Err(error) => {
                                                return self
                                                    .drive_error_outcome_with_items(
                                                        &run_id,
                                                        &mut message,
                                                        &mut reasoning,
                                                        &mut tools,
                                                        error,
                                                    )
                                                    .await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        let promoted = if post_stream_batch {
                            match self.promoted_steers.try_finish_boundary() {
                                Some(promoted) => promoted,
                                None => {
                                    // A live reservation would suspend the
                                    // terminal fence. Restore the historical
                                    // Usage/Completed durability boundaries
                                    // before waiting and disable batching.
                                    if let Err(error) = self
                                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                        .await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                DriveError::from(error),
                                            )
                                            .await;
                                    }
                                    if let Err(error) =
                                        self.complete_text(&run_id, &mut message, false).await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                error,
                                            )
                                            .await;
                                    }
                                    if let Err(error) =
                                        self.complete_text(&run_id, &mut reasoning, true).await
                                    {
                                        return self
                                            .drive_error_outcome_with_items(
                                                &run_id,
                                                &mut message,
                                                &mut reasoning,
                                                &mut tools,
                                                error,
                                            )
                                            .await;
                                    }
                                    post_stream_batch = false;
                                    self.promoted_steers.finish_boundary().await
                                }
                            }
                        } else {
                            self.promoted_steers.finish_boundary().await
                        };
                        if !promoted.is_empty() {
                            // MUTATION CHECK: this is the final atomic fence
                            // between provider Finish and durable Done. A
                            // queue promotion reserved before this point must
                            // force another request; deleting this check makes
                            // the finish-boundary promotion pin lose text.
                            if let Err(error) =
                                finalize_request_usage(&mut completed_usage, &mut request_usage)
                            {
                                return self
                                    .drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        error,
                                    )
                                    .await;
                            }
                            if post_stream_batch {
                                if let Err(error) = self
                                    .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                    .await
                                {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            DriveError::from(error),
                                        )
                                        .await;
                                }
                                if let Err(error) =
                                    self.complete_text(&run_id, &mut message, false).await
                                {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                                if let Err(error) =
                                    self.complete_text(&run_id, &mut reasoning, true).await
                                {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            error,
                                        )
                                        .await;
                                }
                            }
                            messages.extend(promoted.into_iter().map(Message::user_text));
                            provider_attempt = 0;
                            thinking_pending = true;
                            replay.reset_for_next_request();
                            refusal_reason.clear();
                            assistant_blocks.clear();
                            tool_results.clear();
                            continue 'requests;
                        }
                        if post_stream_batch {
                            // Cancellation owns the last synchronous seam
                            // before the one terminal append. If it has won,
                            // preserve Usage and use normal item cleanup.
                            if cancel.is_cancelled() {
                                if let Err(error) = self
                                    .commit_pending_usage(&run_id, &mut pending_usage_commit)
                                    .await
                                {
                                    return self
                                        .drive_error_outcome_with_items(
                                            &run_id,
                                            &mut message,
                                            &mut reasoning,
                                            &mut tools,
                                            DriveError::from(error),
                                        )
                                        .await;
                                }
                                return self
                                    .cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await;
                            }
                            // Every continuation fence has now closed. Release
                            // only this request's replay copy before the
                            // terminal append builds the same text into its
                            // required Item and Node facts.
                            if let Some(index) = current_assistant_message_index {
                                messages.truncate(index);
                            }
                            return match self
                                .commit_post_stream_facts(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut pending_usage_commit,
                                )
                                .await
                            {
                                Ok(()) => TurnOutcome {
                                    state: RunState::Done,
                                    finish_reason: reason,
                                    error: None,
                                },
                                Err(error) => {
                                    self.drive_error_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                        DriveError::from(error),
                                    )
                                    .await
                                }
                            };
                        }
                        return self.finish_outcome(&run_id, reason).await;
                    }
                };

                match event_result {
                    Ok(Some(result)) => tool_results.push(result),
                    Ok(None) => {}
                    Err(DriveError::Cancelled) => {
                        return self
                            .cancelled_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                            )
                            .await;
                    }
                    Err(error) => {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                error,
                            )
                            .await;
                    }
                }
                if let Some(event) = route_replay_event {
                    let marker = serde_json::json!({
                        "response_epoch": replay.response_epoch,
                        "stream_event": event,
                    });
                    if let Err(error) = self
                        .commit_hidden_extension_marker(
                            &run_id,
                            ROUTE_REPLAY_EVENT_EXTENSION_KIND,
                            marker,
                        )
                        .await
                    {
                        return self.errored_state_outcome(&run_id, error).await;
                    }
                    if is_structured_replay_event(&event) {
                        replay.record_structured(event);
                    }
                }
                if cancel.is_cancelled() {
                    if let Err(error) = self
                        .commit_pending_usage(&run_id, &mut pending_usage_commit)
                        .await
                    {
                        return self
                            .drive_error_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                                DriveError::from(error),
                            )
                            .await;
                    }
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
            }
        }
    }

    /// Parks one broken network request until the platform route seam is no
    /// longer authoritatively negative or the original run deadline expires.
    async fn wait_for_provider_route(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        provider: &Arc<dyn Provider>,
    ) -> Result<(), DriveError> {
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        if provider.route_status() != haider_platform::RouteStatus::Unavailable {
            self.commit_state(run_id, RunState::Thinking)
                .await
                .map_err(DriveError::Store)?;
            return Ok(());
        }
        self.commit_state(
            run_id,
            RunState::Waiting {
                reason: WaitReason::NetworkUnavailable,
            },
        )
        .await
        .map_err(DriveError::Store)?;

        let started = tokio::time::Instant::now();
        let provider_deadline = self.config.provider_deadline;
        let budget = provider_deadline
            .map(|deadline| deadline.saturating_duration_since(started))
            .unwrap_or(std::time::Duration::MAX);
        let mut deadline = Box::pin(async move {
            match provider_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        });
        let mut poll = tokio::time::interval_at(
            tokio::time::Instant::now() + ROUTE_STATE_POLL_INTERVAL,
            ROUTE_STATE_POLL_INTERVAL,
        );
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(DriveError::Cancelled),
                () = &mut deadline => {
                    return Err(DriveError::Provider(deadline_exhausted_error(
                        budget,
                        started.elapsed(),
                    )));
                }
                _ = poll.tick() => {
                    // Negative-only attribution: only a confirmed absence
                    // keeps the route wait parked. Available or Unknown lets
                    // the same absolute-deadline request resume.
                    if provider.route_status() != haider_platform::RouteStatus::Unavailable {
                        self.commit_state(run_id, RunState::Thinking)
                            .await
                            .map_err(DriveError::Store)?;
                        return Ok(());
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        return Err(DriveError::Provider(provider_protocol_error(
                            "session actor command channel closed while waiting for network route",
                        )));
                    };
                    if let ActorCommand::Stop { completed } = command {
                        cancel.cancel();
                        let _ = completed.send(());
                        return Err(DriveError::Cancelled);
                    }
                    self.service_command_without_menu(command);
                }
            }
        }
    }

    /// Opens the text/reasoning item on its first delta, then commits the delta.
    async fn apply_text_delta(
        &mut self,
        run_id: &RunId,
        accumulator: &mut Option<TextAccumulator>,
        text: ReplyText,
        reasoning: bool,
    ) -> Result<ReplyText, DriveError> {
        let open = match accumulator.take() {
            Some(open) => open,
            None => {
                // Resolve the eventual assistant node's parent before any
                // terminal Usage can be produced. The post-stream batch may
                // then build every fact synchronously up to its one append.
                if !reasoning {
                    self.tree_parent().await.map_err(DriveError::Store)?;
                }
                let item_id = self.next_item_id();
                let empty = if reasoning {
                    TurnItem::Reasoning {
                        summary: String::new().into(),
                    }
                } else {
                    TurnItem::AgentMessage {
                        text: String::new().into(),
                    }
                };
                self.commit_item(
                    run_id,
                    ItemEvent::Started {
                        item_id: item_id.clone(),
                        item: empty,
                    },
                )
                .await
                .map_err(DriveError::Store)?;
                TextAccumulator::new(item_id, !reasoning)
            }
        };

        let active = accumulator.insert(open);
        let appended = active.append_shared(&text);
        let accumulated = active.snapshot();
        let delta = if reasoning {
            ItemDelta::Reasoning { text: appended }
        } else {
            ItemDelta::Text { text: appended }
        };
        self.commit_item(
            run_id,
            ItemEvent::Delta {
                item_id: active.item_id.clone(),
                delta,
            },
        )
        .await
        .map_err(DriveError::Store)?;
        Ok(accumulated)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_pre_first_event_retry(
        &mut self,
        context: ProviderRetryContext<'_>,
        provider_attempt: &mut usize,
        provider: &mut Arc<dyn Provider>,
        account: &mut Option<CredentialAlias>,
        messages: &mut Vec<Message>,
        stable_history_end: &mut usize,
        current_turn_start: &mut usize,
        latest_compaction_summary_end: &mut Option<usize>,
        rotation_budget_consumed: &mut bool,
        capability_fallback_consumed: &mut bool,
        provider_pair_switch_ordinal: &mut u32,
        thinking_pending: &mut bool,
        mut error: ProviderError,
    ) -> Result<(), DriveError> {
        // A deadline that was already produced by an active request keeps its
        // in-flight classification. Every other pre-first-event failure enters
        // retry admission before resolver and backoff policy are consulted.
        if error.timeout_reason != Some(ProviderTimeoutReason::DeadlineExhausted) {
            self.provider_deadline_state.begin_retry_admission();
        }
        let hosted_fallback = error.presentation.subcode.as_str() == "provider-web-tool-rejected"
            && !*capability_fallback_consumed;
        if ((!*rotation_budget_consumed && provider_error_allows_rotation(&error))
            || hosted_fallback)
            && let (Some(resolver), Some(current_account)) = (
                self.config.provider_attempt_resolver.clone(),
                account.clone(),
            )
        {
            let resolution = tokio::select! {
                biased;
                () = context.cancel.cancelled() => return Err(DriveError::Cancelled),
                resolution = resolver.resolve(&current_account, &error) => resolution,
            }
            .map_err(DriveError::Account)?;
            match resolution {
                ProviderAttemptDecision::Fallback {
                    provider: fallback,
                    account: fallback_account,
                } => {
                    if !hosted_fallback || fallback_account != current_account {
                        return Err(DriveError::Provider(provider_protocol_error(
                            "provider capability fallback returned inconsistent coordinates",
                        )));
                    }
                    if !self.config.has_provider_tool_fallback() {
                        return Err(DriveError::Provider(error));
                    }
                    self.commit_ui_extension_marker(
                        context.run_id,
                        "provider_tool_fallback",
                        serde_json::json!({
                            "label": "provider hosted web tool rejected — using local web_fetch",
                            "attempt": 1,
                            "max_attempts": 1,
                        }),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    *provider = fallback;
                    *account = Some(fallback_account);
                    self.config.activate_provider_tool_fallback();
                    *capability_fallback_consumed = true;
                    return Ok(());
                }
                ProviderAttemptDecision::Retry {
                    provider: refreshed,
                    account: refreshed_account,
                } => {
                    if refreshed_account != current_account {
                        return Err(DriveError::Provider(provider_protocol_error(
                            "credential refresh changed account without a rotation event",
                        )));
                    }
                    // H2: BUDGET the credential refresh under the same
                    // `MAX_API_RETRIES` cap as an ordinary retry. Without this
                    // gate a resolver that keeps deciding `Retry` on a
                    // persistently-failing 401 loops forever — the arm returned
                    // Ok before the cap check below was ever reached. Once the
                    // attempt budget is spent, DON'T refresh again: fall through
                    // to the capped-retry / Errored path so a non-recovering
                    // 401 terminates. The legitimate refresh-then-succeed path
                    // (a refresh at a low attempt count) is unaffected.
                    if *provider_attempt < MAX_API_RETRIES {
                        *provider = refreshed;
                        *account = Some(refreshed_account);
                        return Ok(());
                    }
                }
                ProviderAttemptDecision::Rotate(resolved) => {
                    if resolved.account != resolved.rotation.to
                        || resolved.rotation.from != current_account
                    {
                        return Err(DriveError::Provider(provider_protocol_error(
                            "attempt resolver returned inconsistent rotation coordinates",
                        )));
                    }
                    self.commit_payload(
                        context.run_id,
                        EventPayload::Rotation(resolved.rotation),
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    *provider = resolved.provider;
                    *account = Some(resolved.account);
                    *rotation_budget_consumed = true;
                    return Ok(());
                }
                ProviderAttemptDecision::Switch(target) => {
                    self.commit_provider_pair_switch(
                        context.run_id,
                        target,
                        provider,
                        account,
                        messages,
                        stable_history_end,
                        current_turn_start,
                        latest_compaction_summary_end,
                        provider_pair_switch_ordinal,
                    )
                    .await?;
                    *provider_attempt = 0;
                    *rotation_budget_consumed = true;
                    return Ok(());
                }
                ProviderAttemptDecision::Wait => {
                    *rotation_budget_consumed = true;
                }
                ProviderAttemptDecision::Stop => {
                    *rotation_budget_consumed = true;
                    if provider_error_allows_pair_fallback(&error) {
                        let fallback = tokio::select! {
                            biased;
                            () = context.cancel.cancelled() => return Err(DriveError::Cancelled),
                            resolution = resolver.resolve_fallback(&current_account, &error) => resolution,
                        }
                        .map_err(DriveError::Account)?;
                        if let ProviderAttemptDecision::Switch(target) = fallback {
                            return self
                                .commit_provider_pair_switch(
                                    context.run_id,
                                    target,
                                    provider,
                                    account,
                                    messages,
                                    stable_history_end,
                                    current_turn_start,
                                    latest_compaction_summary_end,
                                    provider_pair_switch_ordinal,
                                )
                                .await
                                .inspect(|()| *provider_attempt = 0);
                        }
                    }
                    return Err(DriveError::Provider(error));
                }
            }
        }
        if provider_error_allows_retry(
            &mut error,
            self.config.provider_deadline,
            context.run_id,
            *provider_attempt,
        ) && *provider_attempt < MAX_API_RETRIES
        {
            self.wait_before_provider_retry(
                context.run_id,
                context.cancel,
                *provider_attempt,
                &error,
            )
            .await?;
            *thinking_pending = true;
            Ok(())
        } else if provider_error_allows_pair_fallback(&error)
            && let (Some(resolver), Some(current_account)) = (
                self.config.provider_attempt_resolver.clone(),
                account.clone(),
            )
        {
            let fallback = tokio::select! {
                biased;
                () = context.cancel.cancelled() => return Err(DriveError::Cancelled),
                resolution = resolver.resolve_fallback(&current_account, &error) => resolution,
            }
            .map_err(DriveError::Account)?;
            match fallback {
                ProviderAttemptDecision::Switch(target) => self
                    .commit_provider_pair_switch(
                        context.run_id,
                        target,
                        provider,
                        account,
                        messages,
                        stable_history_end,
                        current_turn_start,
                        latest_compaction_summary_end,
                        provider_pair_switch_ordinal,
                    )
                    .await
                    .inspect(|()| *provider_attempt = 0),
                ProviderAttemptDecision::Retry { .. }
                | ProviderAttemptDecision::Fallback { .. }
                | ProviderAttemptDecision::Rotate(_)
                | ProviderAttemptDecision::Wait
                | ProviderAttemptDecision::Stop => Err(DriveError::Provider(error)),
            }
        } else {
            Err(DriveError::Provider(error))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_provider_pair_switch(
        &mut self,
        run_id: &RunId,
        target: ProviderPairSwitchTarget,
        provider: &mut Arc<dyn Provider>,
        account: &mut Option<CredentialAlias>,
        messages: &mut Vec<Message>,
        stable_history_end: &mut usize,
        current_turn_start: &mut usize,
        latest_compaction_summary_end: &mut Option<usize>,
        provider_pair_switch_ordinal: &mut u32,
    ) -> Result<(), DriveError> {
        let Some(committer) = self.config.provider_pair_switch_committer.clone() else {
            return Err(DriveError::Provider(provider_protocol_error(
                "automatic provider/model switch has no durable committer",
            )));
        };
        let next_switch_ordinal = provider_pair_switch_ordinal.checked_add(1).ok_or_else(|| {
            DriveError::Provider(provider_protocol_error(
                "automatic provider/model switch ordinal overflowed",
            ))
        })?;
        let switch = ProviderPairSwitch {
            run_id: run_id.clone(),
            switch_ordinal: *provider_pair_switch_ordinal,
            from_provider: self.config.usage_scope.provider.clone(),
            from_model: self.config.model.clone(),
            to_provider: target.provider_name.clone(),
            to_model: target.model.clone(),
            cause: target.cause,
        };
        if switch.from_provider == switch.to_provider && switch.from_model == switch.to_model {
            return Err(DriveError::Provider(provider_protocol_error(
                "automatic provider/model switch did not change the active pair",
            )));
        }
        committer.commit(&switch).await.map_err(DriveError::Store)?;
        *provider_pair_switch_ordinal = next_switch_ordinal;

        remap_after_provider_opaque_strip(
            messages,
            &target.provider_name,
            stable_history_end,
            current_turn_start,
            latest_compaction_summary_end,
        );

        let previous_scope = self.config.usage_scope.clone();
        // A pre-first-event provider failure may not have emitted usage for
        // this turn, so the scope's carried stable count can still be zero.
        // Measure the actor's live immutable boundary before changing the
        // pair; this is the cache prefix the automatic switch actually
        // invalidates.
        let invalidated_stable_tokens = estimated_request_input_tokens(
            &self.config,
            &messages[..(*stable_history_end).min(messages.len())],
        );
        self.config.model = target.model.clone();
        self.config.context_window = target.context_window;
        self.config.cached_input_is_subset = target.cached_input_is_subset;
        self.config
            .install_provider_derived_request_state(&target.provider_request_state);
        self.config.cache_expected_later_reads =
            u32::from(!self.config.tool_definitions().is_empty()) * 2;
        self.config.usage_account = Some(target.account.clone());
        self.config.usage_scope.provider = target.provider_name.clone();
        self.config.usage_scope.model = target.model.clone();
        self.config.usage_scope.account_scope = Some(target.account.clone());
        self.config.usage_scope.auth_scope = target.auth_scope.clone();
        let dimensions = target.provider.usage_lane_dimensions();
        self.config.usage_scope.api_family = dimensions.api_family;
        self.config.usage_scope.effort = dimensions.effort;
        self.config.usage_scope.speed = dimensions.speed;
        let tool_pack_digest = self.config.canonical_tool_pack_digest();
        if let Some(boundaries) = self.config.usage_scope.cache_boundaries.as_mut() {
            boundaries.tool_pack = tool_pack_digest.clone();
        }
        self.config.usage_scope.cache_epoch = digest_json(&serde_json::json!({
            "provider": target.provider_name,
            "model": target.model,
            "account": target.account,
            "auth": target.auth_scope,
            "reasoning": self.config.reasoning_settings,
            "system": digest_json(&self.config.system_prompt),
            "tools": tool_pack_digest,
        }));
        self.config.cache_reuse_gap_ms = None;
        // A compactor is bound to the provider/model used to construct it.
        // Continuing with that stale lane would be a silent reverse switch.
        self.config.context_compactor = None;
        self.config.provider_attempt_resolver = target.attempt_resolver;
        *provider = target.provider;
        *account = Some(target.account.clone());

        let changed_fields = [
            (switch.from_provider != switch.to_provider).then_some("provider"),
            (switch.from_model != switch.to_model).then_some("model"),
            (previous_scope.auth_scope != self.config.usage_scope.auth_scope).then_some("auth"),
            (previous_scope.account_scope != self.config.usage_scope.account_scope)
                .then_some("account"),
        ]
        .into_iter()
        .flatten()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let transition = CacheEpochTransitionV1 {
            reason: CacheEpochTransitionReason::ConfigurationChanged,
            planned: false,
            changed_fields,
            invalidated_stable_tokens,
            rewarm_cost_usd: None,
            rewarm_base_input_equivalent_tokens: None,
            transition_id: digest_json(&serde_json::json!({
                "cause": switch.cause.as_str(),
                "from_provider": switch.from_provider,
                "from_model": switch.from_model,
                "to_provider": switch.to_provider,
                "to_model": switch.to_model,
            })),
            // The exact request epoch includes provider-rendered digests and
            // the live compaction boundary, which do not exist until the next
            // request is assembled. Omitting the endpoints is honest; the
            // named transition itself still records the cold boundary.
            from_cache_epoch: None,
            to_cache_epoch: None,
        };
        let transition_item = transition.extension_item().map_err(|error| {
            DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                format!("cannot serialize automatic cache epoch transition: {error}"),
                false,
            ))
        })?;
        let TurnItem::Extension { kind, data } = transition_item else {
            unreachable!("cache transition always uses the extension carrier")
        };
        self.commit_ui_extension_marker(run_id, &kind, data)
            .await
            .map_err(DriveError::Store)?;
        self.commit_ui_extension_marker(
            run_id,
            "provider_pair_switch_v1",
            serde_json::json!({
                "from_provider": switch.from_provider,
                "from_model": switch.from_model,
                "to_provider": switch.to_provider,
                "to_model": switch.to_model,
                "why": switch.cause.as_str(),
            }),
        )
        .await
        .map_err(DriveError::Store)
    }

    fn reset_cache_boundaries_after_compaction(
        covered_message_count: usize,
        stable_history_end: &mut usize,
        current_turn_start: &mut usize,
        latest_compaction_summary_end: &mut Option<usize>,
    ) {
        let summary_end = 1_usize;
        *stable_history_end =
            summary_end.saturating_add(stable_history_end.saturating_sub(covered_message_count));
        *current_turn_start =
            summary_end.saturating_add(current_turn_start.saturating_sub(covered_message_count));
        *latest_compaction_summary_end = Some(summary_end);
    }

    async fn force_context_compaction(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        stable_history_end: &mut usize,
        current_turn_start: &mut usize,
        latest_compaction_summary_end: &mut Option<usize>,
        forced_compaction_used: &mut bool,
    ) -> Result<(), DriveError> {
        if *forced_compaction_used {
            return Err(repeated_context_overflow_after_compaction());
        }
        let covered_message_count = self
            .perform_context_compaction(
                run_id,
                messages,
                *current_turn_start,
                *latest_compaction_summary_end,
            )
            .await?;
        Self::reset_cache_boundaries_after_compaction(
            covered_message_count,
            stable_history_end,
            current_turn_start,
            latest_compaction_summary_end,
        );
        *forced_compaction_used = true;
        Ok(())
    }

    async fn perform_context_compaction(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        current_turn_start: usize,
        latest_compaction_summary_end: Option<usize>,
    ) -> Result<usize, DriveError> {
        if current_turn_start == 0 || current_turn_start > messages.len() {
            return Err(DriveError::Provider(ProviderError::new(
                ProviderErrorKind::ContextExceeded,
                "provider context overflow has no prior history prefix to compact",
            )));
        }
        let compactor = self.config.context_compactor.clone().ok_or_else(|| {
            DriveError::Provider(ProviderError::new(
                ProviderErrorKind::ContextExceeded,
                "provider context overflow requires a configured context compactor",
            ))
        })?;
        let planned = compactor
            .plan(
                run_id,
                CompactionResume::AutoMidTurn,
                messages,
                current_turn_start,
            )
            .await
            .map_err(DriveError::Store)?;
        if planned.covered_message_count == 0 || planned.covered_message_count > current_turn_start
        {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "context compactor returned an invalid clean message boundary",
                false,
            )));
        }
        let intent = planned.intent;
        self.commit_ui_extension_marker(
            run_id,
            COMPACTION_INTENT_EXTENSION_KIND,
            serde_json::to_value(&intent).map_err(|error| {
                DriveError::Store(HaiderError::new(
                    ErrorCode::Internal,
                    format!("compaction intent could not serialize: {error}"),
                    false,
                ))
            })?,
        )
        .await
        .map_err(DriveError::Store)?;
        self.commit_state(run_id, RunState::Compacting)
            .await
            .map_err(DriveError::Store)?;

        let suffix = messages.split_off(planned.covered_message_count);
        let mut covered = std::mem::take(messages);
        // Round 5: resolve tool-result images exactly as the live lane does
        // — the compactor's replay must be the bytes the provider actually
        // saw, attachments included.
        let attachments = self
            .resolve_tool_result_images(&mut covered)
            .await
            .map_err(DriveError::Store)?;
        let compacted = compactor
            .compact(ContextCompactionRequest {
                run_id,
                intent: &intent,
                covered_messages: covered,
                retained_messages: suffix.clone(),
                attachments,
                latest_compaction_summary_end,
                economy_before: &self.config.context_economy,
            })
            .await
            .map_err(DriveError::from)?;
        self.config.context_economy = compacted.economy;
        messages.push(compacted.summary);
        messages.extend(suffix);
        // The daemon committed a new compaction node behind this actor's
        // cached parent. Reload before the next current-run node is appended
        // so later output descends from the projection switch.
        self.tree_head = None;
        self.tree_head_initialized = false;
        // The compactor atomically commits its final overlay/item together
        // with this resumed state; mirror that durable fact into the watch.
        self.state.send_replace(Some(RunState::Thinking));
        Ok(planned.covered_message_count)
    }

    /// Publishes request occupancy and enforces the daemon-pinned soft/hard
    /// context policy immediately before every provider request. Unknown
    /// catalog windows publish honest estimates but disable proactive
    /// compaction; provider-reported overflow can still force recovery.
    #[allow(clippy::too_many_arguments)]
    async fn enforce_context_policy(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        current_turn_start: &mut usize,
        latest_compaction_summary_end: &mut Option<usize>,
        volatile_user_tail: Option<&str>,
        provider: &mut Arc<dyn Provider>,
        account: &mut Option<CredentialAlias>,
        stable_history_end: &mut usize,
        compaction_guard_consumed: &mut bool,
        provider_pair_switch_ordinal: &mut u32,
        thinking_pending: &mut bool,
    ) -> Result<bool, DriveError> {
        // Volatile context is excluded from durable cache boundaries, but it
        // still consumes real provider input capacity. Measure a request-only
        // projection so the hard-fit policy remains honest without allowing
        // the tail into compaction or journal history.
        let mut before =
            estimated_request_shaped_context_footprint(&self.config, messages, volatile_user_tail);
        if self.config.context_compaction_v1
            && self.config.structural_context_trimming
            && let Some(window) = self.config.context_window
        {
            let tier_two = context_tier_threshold_tokens(
                window,
                self.config.reserved_output_tokens,
                CONTEXT_STRUCTURAL_TIER_TWO_PERCENT,
            );
            let tier_one = context_tier_threshold_tokens(
                window,
                self.config.reserved_output_tokens,
                CONTEXT_STRUCTURAL_TIER_ONE_PERCENT,
            );
            let structural_tier =
                if tier_two.is_some_and(|threshold| before.used_tokens >= threshold) {
                    Some((
                        ContextCompactionTier::StructuralTrim12,
                        CONTEXT_STRUCTURAL_TIER_TWO_RETAINED_TOOL_PAIRS,
                    ))
                } else if tier_one.is_some_and(|threshold| before.used_tokens >= threshold) {
                    Some((
                        ContextCompactionTier::StructuralTrim24,
                        CONTEXT_STRUCTURAL_TIER_ONE_RETAINED_TOOL_PAIRS,
                    ))
                } else {
                    None
                };
            if let Some((tier, retained_pairs)) = structural_tier {
                let estimated_tokens_before =
                    estimated_request_savings_tokens(&self.config, messages, volatile_user_tail);
                let trimmed = trim_stale_tool_pairs(messages, *current_turn_start, retained_pairs);
                if trimmed.removed_pairs > 0 {
                    *current_turn_start = current_turn_start
                        .saturating_sub(trimmed.removed_messages_before_protected_start);
                    *stable_history_end = stable_history_end
                        .saturating_sub(trimmed.removed_messages_before_protected_start);
                    *latest_compaction_summary_end =
                        latest_compaction_summary_end.map(|boundary| {
                            boundary.saturating_sub(
                                trimmed
                                    .removed_messages_before_protected_start
                                    .min(boundary),
                            )
                        });
                    let estimated_tokens_after = estimated_request_savings_tokens(
                        &self.config,
                        messages,
                        volatile_user_tail,
                    );
                    self.commit_pending_thinking(run_id, thinking_pending)
                        .await
                        .map_err(DriveError::Store)?;
                    self.commit_context_savings(
                        run_id,
                        tier,
                        estimated_tokens_before,
                        estimated_tokens_after,
                        trimmed.removed_tool_call_ids,
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    before = estimated_request_shaped_context_footprint(
                        &self.config,
                        messages,
                        volatile_user_tail,
                    );
                }
            }
        }
        if self.config.context_compaction_v1 {
            self.commit_pending_thinking(run_id, thinking_pending)
                .await
                .map_err(DriveError::Store)?;
            self.commit_context_footprint(run_id, &before)
                .await
                .map_err(DriveError::Store)?;
        }
        let Some(window) = self.config.context_window else {
            return Ok(false);
        };
        let input_budget = window
            .checked_sub(self.config.reserved_output_tokens)
            .ok_or_else(|| {
                DriveError::Provider(ProviderError::new(
                    ProviderErrorKind::ContextExceeded,
                    "reserved output budget leaves no provider input capacity",
                ))
            })?;
        if self.config.compaction_guard_v1 && *compaction_guard_consumed {
            return if before.used_tokens > input_budget {
                Err(compaction_guard_repeat_error(
                    before.used_tokens,
                    input_budget,
                ))
            } else {
                // The promoted hard budget absorbs this request. Crossing its
                // soft line cannot trigger a second compaction in this turn.
                Ok(false)
            };
        }
        let should_compact = if self.config.context_compaction_v1 {
            let soft_threshold =
                context_soft_threshold_tokens(window, self.config.reserved_output_tokens)
                    .ok_or_else(|| {
                        DriveError::Provider(ProviderError::new(
                            ProviderErrorKind::ContextExceeded,
                            "reserved output budget leaves no provider input capacity",
                        ))
                    })?;
            before.used_tokens >= soft_threshold
        } else {
            before.used_tokens > input_budget
        };
        if !should_compact {
            return Ok(false);
        }
        self.commit_pending_thinking(run_id, thinking_pending)
            .await
            .map_err(DriveError::Store)?;
        let covered_message_count = self
            .perform_context_compaction(
                run_id,
                messages,
                *current_turn_start,
                *latest_compaction_summary_end,
            )
            .await?;
        Self::reset_cache_boundaries_after_compaction(
            covered_message_count,
            stable_history_end,
            current_turn_start,
            latest_compaction_summary_end,
        );
        let after = estimated_context_footprint(&self.config, messages);
        if self.config.context_compaction_v1 {
            self.commit_context_footprint(run_id, &after)
                .await
                .map_err(DriveError::Store)?;
        }
        let after_for_guard =
            estimated_request_shaped_context_footprint(&self.config, messages, volatile_user_tail);
        if self.config.compaction_guard_v1
            && compaction_guard_tripped(
                before.used_tokens,
                after_for_guard.used_tokens,
                input_budget,
            )
        {
            *compaction_guard_consumed = true;
            let promotion = self.config.compaction_promotion.take().filter(|target| {
                target.cause == ProviderPairSwitchCause::CompactionGuard
                    && target.provider_name == self.config.usage_scope.provider
                    && target
                        .context_window
                        .is_some_and(|target_window| target_window > window)
            });
            let Some(promotion) = promotion else {
                return Err(compaction_runaway_guard_error(
                    before.used_tokens,
                    after_for_guard.used_tokens,
                    input_budget,
                ));
            };
            self.commit_provider_pair_switch(
                run_id,
                promotion,
                provider,
                account,
                messages,
                stable_history_end,
                current_turn_start,
                latest_compaction_summary_end,
                provider_pair_switch_ordinal,
            )
            .await?;
            let promoted = estimated_context_footprint(&self.config, messages);
            if self.config.context_compaction_v1 {
                self.commit_context_footprint(run_id, &promoted)
                    .await
                    .map_err(DriveError::Store)?;
            }
            let promoted_for_fit = estimated_request_shaped_context_footprint(
                &self.config,
                messages,
                volatile_user_tail,
            );
            let promoted_budget = self
                .config
                .context_window
                .and_then(|promoted_window| {
                    promoted_window.checked_sub(self.config.reserved_output_tokens)
                })
                .ok_or_else(|| {
                    compaction_guard_repeat_error(promoted_for_fit.used_tokens, input_budget)
                })?;
            if promoted_for_fit.used_tokens > promoted_budget {
                return Err(compaction_guard_repeat_error(
                    promoted_for_fit.used_tokens,
                    promoted_budget,
                ));
            }
            return Ok(true);
        }
        if after_for_guard.used_tokens > input_budget {
            return Err(DriveError::Provider(ProviderError::new(
                ProviderErrorKind::ContextExceeded,
                format!(
                    "compacted provider input estimate {} exceeds budget {input_budget}",
                    after_for_guard.used_tokens
                ),
            )));
        }
        Ok(true)
    }

    /// Commits adapter-visible `Waiting { reason }` telemetry immediately
    /// before `Retrying { attempt, max, delay_ms, reason }` (W-C M4: the
    /// visible `attempt K/max` counter), then waits through the injected
    /// sleeper. The next request batches its `Thinking` transition with the
    /// durable cache-attempt marker.
    ///
    /// The delay is the run-scoped [`retry_jittered_backoff_ms`] schedule UNLESS the provider
    /// sent a `retry_after_ms`, which OVERRIDES it exactly through the
    /// one-minute respect cap. Instructions beyond the respect cap are
    /// terminalized as retryable exhaustion instead of silently shortened.
    /// The committed `attempt` is `failed_attempt + 1` — the NEXT try — so a
    /// first failure renders `attempt 2` (matching the screenshot).
    async fn wait_before_provider_retry(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        failed_attempt: usize,
        error: &ProviderError,
    ) -> Result<(), DriveError> {
        if error
            .retry_after_ms
            .is_some_and(|delay| delay > MAX_PROVIDER_RETRY_AFTER_MS)
        {
            let capped = ProviderError {
                kind: error.kind,
                message: format!(
                    "provider retry-after {}ms exceeds the {}ms respect cap",
                    error.retry_after_ms.unwrap_or_default(),
                    MAX_PROVIDER_RETRY_AFTER_MS
                ),
                retryable: true,
                retry_after_ms: error.retry_after_ms,
                opened_within_ms: error.opened_within_ms,
                budget_ms: error.budget_ms,
                timeout_reason: error.timeout_reason,
                presentation: error.presentation.clone(),
            };
            return Err(DriveError::Provider(capped));
        }
        let reason = match error.kind {
            ProviderErrorKind::RateLimited => WaitReason::RateLimit,
            _ => WaitReason::ProviderBackoff,
        };
        // `retry_after_ms` (429/529 Retry-After) OVERRIDES the computed
        // backoff; otherwise use the jittered exponential schedule.
        let delay_ms = error
            .retry_after_ms
            .unwrap_or_else(|| retry_jittered_backoff_ms(run_id, failed_attempt));
        let waiting = RunState::Waiting {
            reason: reason.clone(),
        };
        let retrying = RunState::Retrying {
            attempt: u32::try_from(failed_attempt.saturating_add(1)).unwrap_or(u32::MAX),
            max: u32::try_from(MAX_API_RETRIES).unwrap_or(u32::MAX),
            delay_ms,
            reason,
        };
        // Arm against the actor-minted event id BEFORE the append becomes
        // visible. A daemon can therefore never observe this durable
        // `Retrying` fact in the small gap before its wake seam exists.
        let envelopes = [
            self.uncommitted_envelope(
                run_id,
                EventPayload::RunState(waiting),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::RunState(retrying.clone()),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
        ];
        let retrying_event_id = envelopes[1].event_id.clone();
        let retry_wake = Arc::clone(&self.provider_retry_wake);
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        retry_wake.arm(retrying_event_id.clone());
        if let Err(error) = self.append_and_publish_owned(Vec::from(envelopes)).await {
            retry_wake.disarm(&retrying_event_id);
            return Err(DriveError::Store(error));
        }
        self.state.send_replace(Some(retrying));
        // Clone the sleeper Arc into a local so the pinned backoff future
        // borrows IT, not `self` — the loop below services commands through
        // `&mut self` while the same sleep deadline is still pending.
        let sleeper = Arc::clone(&self.config.retry_sleeper);
        let sleep = sleeper.sleep(delay_ms);
        tokio::pin!(sleep);
        let wait_result = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Err(DriveError::Cancelled),
                () = retry_wake.fired(&retrying_event_id) => break Ok(()),
                // L1: a Stop (or a closed command channel) during a long
                // Retry-After backoff must not block shutdown for the full
                // delay — treat it as a cancel. Other commands are serviced and
                // the SAME sleep deadline is re-awaited, so the backoff clock
                // never restarts.
                command = self.commands.recv() => {
                    match command {
                        None => break Err(DriveError::Cancelled),
                        Some(ActorCommand::Stop { completed }) => {
                            cancel.cancel();
                            let _ = completed.send(());
                            break Err(DriveError::Cancelled);
                        }
                        Some(other) => {
                            self.service_command_without_menu(other);
                            continue;
                        }
                    }
                }
                () = &mut sleep => break Ok(()),
            }
        };
        // Disarm synchronously before any await. A wake racing the natural
        // timer after the selected branch is therefore either the one winner
        // above or an idempotent no-op; it can never schedule another try.
        retry_wake.disarm(&retrying_event_id);
        wait_result?;
        if cancel.is_cancelled() {
            return Err(DriveError::Cancelled);
        }
        Ok(())
    }

    /// Commits `Completed` with the accumulated text; no-op when nothing streamed.
    async fn complete_text(
        &mut self,
        run_id: &RunId,
        accumulator: &mut Option<TextAccumulator>,
        reasoning: bool,
    ) -> Result<(), DriveError> {
        let Some(active) = accumulator.as_mut() else {
            return Ok(());
        };
        let text = active.seal();
        let item = if reasoning {
            TurnItem::Reasoning { summary: text }
        } else {
            TurnItem::AgentMessage { text }
        };
        self.commit_item(
            run_id,
            ItemEvent::Completed {
                item_id: active.item_id.clone(),
                item,
            },
        )
        .await
        .map_err(DriveError::Store)?;
        *accumulator = None;
        Ok(())
    }

    /// Closes the currently streamed assistant item without pretending the
    /// provider completed it. The text remains visible and durable, while
    /// prompt replay decides whether it belongs in history from the paired
    /// recovery-menu answer.
    async fn complete_incomplete_message(
        &mut self,
        run_id: &RunId,
        accumulator: &mut Option<TextAccumulator>,
        interruption: ErrorPresentation,
    ) -> Result<(ItemId, ReplyText), DriveError> {
        let active = accumulator.as_mut().ok_or_else(|| {
            DriveError::Provider(provider_protocol_error(
                "partial-stream recovery had no active assistant item",
            ))
        })?;
        let item_id = active.item_id.clone();
        let text = active.seal();
        self.commit_item(
            run_id,
            ItemEvent::Completed {
                item_id: item_id.clone(),
                item: TurnItem::IncompleteAgentMessage {
                    text: text.clone(),
                    interruption,
                },
            },
        )
        .await
        .map_err(DriveError::Store)?;
        *accumulator = None;
        Ok((item_id, text))
    }

    async fn recover_tool_repair_state(
        &self,
        run_id: &RunId,
        calls: &HashSet<&str>,
    ) -> Result<(HashMap<String, String>, bool), HaiderError> {
        let mut names = HashMap::new();
        let mut pending_repair = false;
        let mut cursor = 0;
        loop {
            let page = self
                .store
                .read_reducer_page(
                    &self.config.session_id,
                    cursor,
                    256,
                    1024 * 1024,
                    &["item", "tool_result"],
                )
                .await?;
            if page.is_empty() {
                return Ok((names, pending_repair));
            }
            for event in page {
                cursor = event.seq;
                if event.run_id.as_ref() != Some(run_id) {
                    continue;
                }
                let Ok(payload) = event.payload.decode_event() else {
                    continue;
                };
                match payload {
                    EventPayload::ToolResult { result, .. }
                        if invalid_tool_call_result(&result) =>
                    {
                        pending_repair = true
                    }
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, .. },
                        ..
                    }) if kind == TOOL_CALL_REPAIR_RESET_EXTENSION_KIND => pending_repair = false,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, data },
                        ..
                    }) if kind == ROUTE_REPLAY_EVENT_EXTENSION_KIND => {
                        if let Some(value) = data.get("stream_event")
                            && let Ok(StreamEvent::ToolCallStart { call_id, name }) =
                                serde_json::from_value(value.clone())
                            && calls.contains(call_id.as_str())
                            && repaired_tool_name(self.config.tool_definitions(), &name).is_some()
                        {
                            names.insert(call_id, name);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    async fn start_tool(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        call_id: String,
        name: String,
    ) -> Result<(), DriveError> {
        if tools.iter().any(|tool| tool.call_id == call_id) {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider started duplicate tool call `{call_id}`",
            ))));
        }
        // Match only one declaration in the actual advertised pack. Exact
        // names win; ambiguous normalized names and unadvertised names are
        // left alone, so normalization cannot widen a grant ceiling.
        let corrected = repaired_tool_name(self.config.tool_definitions(), &name);
        let requested_name = corrected.as_ref().map(|_| name.clone());
        let name = corrected.unwrap_or(name);
        let item_id = self.next_item_id();
        self.commit_item(
            run_id,
            ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: serde_json::json!({}),
                    status: ToolStatus::InProgress,
                },
            },
        )
        .await
        .map_err(DriveError::Store)?;
        tools.push(ToolAccumulator {
            item_id,
            call_id,
            name,
            args: String::new(),
            requested_name,
            parsed_args: OnceLock::new(),
        });
        Ok(())
    }

    async fn apply_tool_delta(
        &mut self,
        run_id: &RunId,
        tools: &mut [ToolAccumulator],
        call_id: &str,
        args_fragment: String,
    ) -> Result<(), DriveError> {
        let Some(tool) = tools.iter_mut().find(|tool| tool.call_id == call_id) else {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider streamed arguments for unknown tool call `{call_id}`",
            ))));
        };
        let _ = tool.parsed_args.take();
        tool.args.push_str(&args_fragment);
        self.commit_item(
            run_id,
            ItemEvent::Delta {
                item_id: tool.item_id.clone(),
                delta: ItemDelta::ToolArgs {
                    fragment: args_fragment,
                },
            },
        )
        .await
        .map_err(DriveError::Store)?;
        Ok(())
    }

    /// Closes a provider-authored tool call whose streamed argument buffer is
    /// not a JSON object. Commit the failed call/result pair before permitting
    /// one repair continuation. Raw arguments remain in the journal; the model
    /// receives an empty object paired with an explicit invalid-call result.
    async fn close_malformed_tool_failure(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        call_id: &str,
        error: &ProviderError,
    ) -> Result<(Block, Message), DriveError> {
        let Some(index) = tools.iter().position(|tool| tool.call_id == call_id) else {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider ended unknown tool call `{call_id}`",
            ))));
        };
        let tool = &tools[index];
        let diagnostic = serde_json::json!({
            "status": "failed",
            "error": {
                "kind": "invalid_tool_call",
                "tool": tool.name,
                "message": error.message,
                "repair": "Resend the tool call with valid JSON object arguments matching its schema. A second consecutive malformed call terminates the run.",
            },
        });
        let result = BoundedResult {
            preview: diagnostic.to_string(),
            truncated: false,
            data: Some(haider_protocol::tool::ToolResultData::InvalidToolCall {
                tool: tool.name.clone(),
                message: error.message.clone(),
            }),
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Failed,
            reason: Some(error.message.clone()),
            presentation: Some(tool_error_presentation(
                "invalid-tool-call",
                "Invalid tool call",
                &error.message,
            )),
        };
        let result = tool.correct_result(result);
        self.commit_tool_result_and_completion(run_id, tool, &result)
            .await?;
        let block = Block::ToolCall {
            call_id: tool.call_id.clone(),
            name: tool.name.clone(),
            args: serde_json::json!({}),
        };
        let message = Message::tool_result(tool.call_id.clone(), result.preview, false);
        tools.remove(index);
        Ok((block, message))
    }

    /// Closes the matching tool item for a provider `ToolCallEnd`.
    async fn complete_tool(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        deferred: &mut Vec<DeferredAccumulator>,
        call_id: &str,
        cancel: &CancelToken,
    ) -> Result<Option<Message>, DriveError> {
        let Some(index) = tools.iter().position(|tool| tool.call_id == call_id) else {
            return Err(DriveError::Provider(provider_protocol_error(format!(
                "provider ended unknown tool call `{call_id}`",
            ))));
        };
        if let Some(dispatcher) = self.dispatcher.as_ref() {
            dispatcher
                .preflight_tool_call(&tools[index].name)
                .await
                .map_err(DriveError::Store)?;
        }
        // A delegated child or daemon-owned workflow may only invoke
        // declarations present in its resolved grant-filtered pack. This
        // actor-side fence covers request/plan/todo before the general
        // dispatcher is reached.
        if !tool_call_within_advertised_ceiling(&self.config, &tools[index].name) {
            let authority = if self.config.agent_id.is_some() {
                "child"
            } else {
                "workflow"
            };
            let reason = format!(
                "grant ceiling violation: {authority} is not allowed to use `{}`",
                tools[index].name
            );
            let result = BoundedResult {
                preview: serde_json::json!({
                    "status": "rejected",
                    "error": {
                        "kind": "grant_ceiling_violation",
                        "message": reason,
                    }
                })
                .to_string(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Rejected,
                reason: Some(reason),
                presentation: Some(tool_error_presentation(
                    "grant-ceiling-violation",
                    "Tool grant denied",
                    &format!("This {authority} is not allowed to use the requested tool."),
                )),
            };
            let result = tools[index].correct_result(result);
            let call_id = tools[index].call_id.clone();
            self.commit_tool_result_and_completion(run_id, &tools[index], &result)
                .await?;
            tools.remove(index);
            return Ok(Some(Message::tool_result(call_id, result.preview, false)));
        }
        if tools[index].name == "request_input" || tools[index].name == "plan" {
            return self
                .complete_request_input(run_id, tools, index, cancel)
                .await
                .map(Some);
        }
        if tools[index].name == "todo_write" {
            return self
                .complete_todo_write(run_id, tools, index)
                .await
                .map(Some);
        }
        if let Some(dispatcher) = self.dispatcher.as_ref().map(Arc::clone) {
            let args = parse_tool_args(&tools[index])?;
            self.commit_state(run_id, RunState::RunningTool)
                .await
                .map_err(DriveError::Store)?;
            let outcome = self
                .execute_general_tool(run_id, &tools[index], args, cancel, &dispatcher)
                .await?;
            let result = match outcome {
                GeneralToolOutcome::Completed(result) => result,
                GeneralToolOutcome::Deferred(ticket) => {
                    self.commit_payload(
                        run_id,
                        EventPayload::AgentSpawned(ticket.manifest.clone()),
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    self.commit_payload(
                        run_id,
                        EventPayload::AgentChipState {
                            agent: ticket.manifest.agent.clone(),
                            chip: ChipState::Thinking,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    self.commit_closed_item(
                        run_id,
                        TurnItem::ChildSpawn {
                            agent: ticket.manifest.agent.clone(),
                        },
                    )
                    .await?;
                    deferred.push(DeferredAccumulator {
                        call_id: tools[index].call_id.clone(),
                        ticket: *ticket,
                        report_emitted: false,
                        child_result_emitted: false,
                        tool_result_emitted: false,
                        item_completed: false,
                    });
                    self.commit_state(run_id, RunState::Streaming)
                        .await
                        .map_err(DriveError::Store)?;
                    return Ok(None);
                }
            };
            let result = tools[index].correct_result(result);
            self.admit_tool_result_images(&result.images).await?;
            let call_id = tools[index].call_id.clone();
            self.commit_tool_settlement_and_streaming(run_id, &tools[index], &result)
                .await?;
            let projection = model_tool_result_projection(&tools[index].name, &result);
            tools.remove(index);
            return Ok(Some(Message::tool_result_with_images(
                call_id,
                projection.preview,
                projection.truncated,
                result.images,
            )));
        }
        self.commit_tool_completed(run_id, &tools[index], ToolStatus::Pending)
            .await?;
        tools.remove(index);
        Ok(None)
    }

    async fn execute_general_tool(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        args: Arc<serde_json::Value>,
        cancel: &CancelToken,
        dispatcher: &Arc<dyn ToolDispatcher>,
    ) -> Result<GeneralToolOutcome, DriveError> {
        loop {
            let execution = dispatcher.execute_shared(
                run_id,
                &tool.item_id,
                &tool.call_id,
                &tool.name,
                Arc::clone(&args),
                cancel,
            );
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                result = execution => Some(result),
            };
            let Some(result) = result else {
                // Drop the losing execution future first. Process-backed
                // dispatchers use that drop as their cancellation hand-off;
                // closing here drains the supervisor before core durably
                // completes the tool item as Cancelled, so no output delta can
                // follow the item's terminal event. Close errors cannot turn a
                // committed user cancellation into a failure; the daemon owner
                // repeats/logs its idempotent close after the actor settles.
                let _ = dispatcher.cancel().await;
                return Err(DriveError::Cancelled);
            };
            let result = result.map_err(DriveError::Store)?;
            match result {
                ToolDispatchResult::Completed(result) => {
                    return Ok(GeneralToolOutcome::Completed(result));
                }
                ToolDispatchResult::Deferred(ticket) => {
                    return Ok(GeneralToolOutcome::Deferred(Box::new(ticket)));
                }
                ToolDispatchResult::ApprovalRequired(menu) => {
                    let opened = self
                        .commit_payload(
                            run_id,
                            EventPayload::MenuOpened(menu.clone()),
                            prompt_omit_render(),
                        )
                        .await
                        .map_err(DriveError::Store)?;
                    self.commit_state(
                        run_id,
                        RunState::PermissionRequired {
                            menu: menu.id.clone(),
                        },
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    let checkpoint = RequestInputCheckpoint {
                        menu: menu.clone(),
                        request_seq: opened.seq,
                        opening_generation: opened.worker_generation,
                        tool_item_id: tool.item_id.clone(),
                        call_id: tool.call_id.clone(),
                        tool_name: tool.name.clone(),
                        args: serde_json::to_string(args.as_ref()).map_err(|error| {
                            DriveError::Store(HaiderError::new(
                                ErrorCode::Internal,
                                format!("tool approval arguments could not serialize: {error}"),
                                false,
                            ))
                        })?,
                    };
                    dispatcher
                        .activate_approval(run_id, &checkpoint)
                        .await
                        .map_err(DriveError::Store)?;
                    let answer = self
                        .wait_for_permission_answer(run_id, cancel, &menu)
                        .await?;
                    dispatcher
                        .resolve_approval(&menu, &answer)
                        .await
                        .map_err(DriveError::Store)?;
                    self.commit_state(run_id, RunState::RunningTool)
                        .await
                        .map_err(DriveError::Store)?;
                }
            }
        }
    }

    /// Waits only for the daemon's committed menu-CAS wake. Unlike
    /// `request_input`, a raw in-process answer must never arm a mutating
    /// effect: the durable CAS commit is the approval credential.
    async fn wait_for_permission_answer(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        menu: &Menu,
    ) -> Result<MenuAnswer, DriveError> {
        loop {
            let wake = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    return Err(DriveError::Cancelled);
                },
                changed = self.committed_menus.changed(),
                    if self.committed_menus.has_changed().is_ok() =>
                {
                    match changed {
                        Ok(()) => self
                            .committed_menus
                            .borrow_and_update()
                            .clone()
                            .map(MenuWake::Committed),
                        Err(_) => None,
                    }
                },
                command = self.commands.recv() => command.map(MenuWake::Command),
            };
            let Some(wake) = wake else {
                return Err(DriveError::Provider(provider_protocol_error(
                    "session actor command channel closed with permission unanswered",
                )));
            };
            match wake {
                MenuWake::Command(command @ ActorCommand::Submit { .. }) => {
                    self.defer_submit_or_reject(command);
                }
                MenuWake::Command(
                    command @ (ActorCommand::Nudge { .. } | ActorCommand::PromotedSteerWake { .. }),
                ) => {
                    self.service_command_without_menu(command);
                }
                MenuWake::Command(ActorCommand::AnswerMenu { completed, .. }) => {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::PermissionDenied,
                        "mutation approval requires the daemon's committed menu CAS",
                        false,
                    )));
                }
                MenuWake::Command(ActorCommand::Stop { completed }) => {
                    cancel.cancel();
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    let _ = completed.send(());
                    return Err(DriveError::Cancelled);
                }
                MenuWake::Committed(envelope) => {
                    let payload = envelope.payload.decode_event().map_err(|error| {
                        DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("committed permission wake could not decode: {error}"),
                            false,
                        ))
                    })?;
                    let EventPayload::MenuAnswered(answer) = payload else {
                        return Err(DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "committed permission wake did not contain MenuAnswered",
                            false,
                        )));
                    };
                    if answer.menu != menu.id {
                        return Err(DriveError::Store(HaiderError::new(
                            ErrorCode::MenuNotFound,
                            format!(
                                "committed answer for menu {} reached permission waiter for {}",
                                answer.menu, menu.id
                            ),
                            false,
                        )));
                    }
                    validate_permission_selection(menu, &answer).map_err(DriveError::Store)?;
                    return Ok(answer);
                }
            }
        }
    }

    /// Waits for the daemon CAS that owns a durable graph-abandon card. The
    /// model cannot answer this menu and core never appends a duplicate fact.
    async fn wait_for_graph_finalization_answer(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        menu: &Menu,
    ) -> Result<GraphFinalizationAnswer, DriveError> {
        loop {
            let wake = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    return Err(DriveError::Cancelled);
                },
                changed = self.committed_menus.changed(),
                    if self.committed_menus.has_changed().is_ok() =>
                {
                    match changed {
                        Ok(()) => self
                            .committed_menus
                            .borrow_and_update()
                            .clone()
                            .map(MenuWake::Committed),
                        Err(_) => None,
                    }
                },
                command = self.commands.recv() => command.map(MenuWake::Command),
            };
            let Some(wake) = wake else {
                return Err(DriveError::Provider(provider_protocol_error(
                    "session actor command channel closed with graph finalization unanswered",
                )));
            };
            let answer = match wake {
                MenuWake::Command(command @ ActorCommand::Submit { .. }) => {
                    self.defer_submit_or_reject(command);
                    continue;
                }
                MenuWake::Command(
                    command @ (ActorCommand::Nudge { .. } | ActorCommand::PromotedSteerWake { .. }),
                ) => {
                    self.service_command_without_menu(command);
                    continue;
                }
                MenuWake::Command(ActorCommand::AnswerMenu { completed, .. }) => {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::PermissionDenied,
                        "graph finalization requires the daemon's committed menu CAS",
                        false,
                    )));
                    continue;
                }
                MenuWake::Command(ActorCommand::Stop { completed }) => {
                    cancel.cancel();
                    let _ = completed.send(());
                    continue;
                }
                MenuWake::Committed(envelope) => {
                    let payload = envelope.payload.decode_event().map_err(|error| {
                        DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("committed graph finalization wake could not decode: {error}"),
                            false,
                        ))
                    })?;
                    match payload {
                        EventPayload::MenuAnswered(answer) => answer,
                        EventPayload::MenuClosed { menu: closed, .. } if closed == menu.id => {
                            return Ok(GraphFinalizationAnswer::Reconsult);
                        }
                        EventPayload::MenuClosed { menu: closed, .. } => {
                            return Err(DriveError::Store(HaiderError::new(
                                ErrorCode::MenuNotFound,
                                format!(
                                    "committed closure for menu {closed} reached graph finalization waiter for {}",
                                    menu.id
                                ),
                                false,
                            )));
                        }
                        _ => {
                            return Err(DriveError::Store(HaiderError::new(
                                ErrorCode::InvalidArgument,
                                "committed graph finalization wake was not a menu resolution",
                                false,
                            )));
                        }
                    }
                }
            };
            if answer.menu != menu.id {
                return Err(DriveError::Store(HaiderError::new(
                    ErrorCode::MenuNotFound,
                    format!(
                        "committed answer for menu {} reached graph finalization waiter for {}",
                        answer.menu, menu.id
                    ),
                    false,
                )));
            }
            return match answer.option_key.as_deref() {
                Some("continue-work") => Ok(GraphFinalizationAnswer::ContinueWork),
                Some("abandon-and-finish") => Ok(GraphFinalizationAnswer::AbandonAndFinish),
                _ => Err(DriveError::Store(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "unsupported graph finalization answer",
                    false,
                ))),
            };
        }
    }

    /// Settles one `todo_write` call synchronously (G1). Like
    /// `request_input`, the tool never enters the effect broker: the actor
    /// itself journals the `TurnItem::Plan` lifecycle facts and answers the
    /// model with a compact count echo. A validation failure is a typed
    /// REJECTED tool result — the model corrects its list — never a turn
    /// failure.
    async fn complete_todo_write(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
    ) -> Result<Message, DriveError> {
        let args = parse_tool_args(&tools[index])?;
        let result = match TodoWrite::from_tool_args(args.as_ref().clone()) {
            Ok(request) => {
                self.emit_plan_facts(run_id, &request).await?;
                BoundedResult {
                    preview: request.result_echo().to_string(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
                }
            }
            Err(error @ haider_tools::ToolError::InvalidArgument { .. }) => BoundedResult {
                preview: serde_json::json!({
                    "status": "rejected",
                    "error": {
                        "kind": "invalid_argument",
                        "message": error.to_string(),
                    }
                })
                .to_string(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Rejected,
                reason: Some(sanitized_failure_message(&error.to_string())),
                presentation: Some(tool_error_presentation(
                    "invalid-tool-argument",
                    "Tool arguments were rejected",
                    "The tool could not accept the supplied arguments.",
                )),
            },
            Err(error) => return Err(tool_error_to_drive(error)),
        };
        let result = tools[index].correct_result(result);
        let call_id = tools[index].call_id.clone();
        self.commit_tool_result_and_completion(run_id, &tools[index], &result)
            .await?;
        tools.remove(index);
        Ok(Message::tool_result_with_images(
            call_id,
            result.preview,
            result.truncated,
            result.images,
        ))
    }

    /// Journals the plan lifecycle for one accepted `todo_write` list.
    ///
    /// First write of a lifecycle: `Started{Plan}` under a FRESH item id
    /// (pins the panel). Every later write: `Completed{Plan}` under the SAME
    /// id — replace semantics; the projection keeps it pinned until every
    /// item completes. An all-completed list closes the lifecycle (the
    /// Completed fact also pairs a `NodeKind::Todos` commit in
    /// `commit_item`); an empty list clears it — and when nothing was ever
    /// listed, an empty list journals NOTHING at all.
    async fn emit_plan_facts(
        &mut self,
        run_id: &RunId,
        request: &TodoWrite,
    ) -> Result<(), DriveError> {
        let open = self
            .plan
            .as_ref()
            .filter(|plan| plan.run_id == *run_id)
            .map(|plan| plan.item_id.clone());
        let item = TurnItem::Plan {
            items: request.items.clone(),
        };
        match open {
            None => {
                if request.items.is_empty() {
                    return Ok(());
                }
                let item_id = self.next_item_id();
                self.commit_item(
                    run_id,
                    ItemEvent::Started {
                        item_id: item_id.clone(),
                        item: item.clone(),
                    },
                )
                .await
                .map_err(DriveError::Store)?;
                if request.all_completed() {
                    // Born finished: close the lifecycle immediately so the
                    // projection unpins it into the transcript and the
                    // history tree records the completed plan.
                    self.commit_item(run_id, ItemEvent::Completed { item_id, item })
                        .await
                        .map_err(DriveError::Store)?;
                    self.plan = None;
                } else {
                    self.plan = Some(PlanLifecycle {
                        run_id: run_id.clone(),
                        item_id,
                    });
                }
            }
            Some(item_id) => {
                self.commit_item(run_id, ItemEvent::Completed { item_id, item })
                    .await
                    .map_err(DriveError::Store)?;
                if request.items.is_empty() || request.all_completed() {
                    // Finished or cleared — the projection closes this item
                    // id forever, so a later write must mint a fresh one.
                    self.plan = None;
                }
            }
        }
        Ok(())
    }

    async fn complete_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
    ) -> Result<Message, DriveError> {
        let args = parse_tool_args(&tools[index])?;
        let name = tools[index].name.clone();
        if name == "plan" {
            let plan = Plan::from_tool_args(args.as_ref().clone()).map_err(tool_error_to_drive)?;
            let menu = plan.menu(self.next_menu_id());
            self.commit_payload(
                run_id,
                EventPayload::MenuOpened(menu.clone()),
                prompt_omit_render(),
            )
            .await
            .map_err(DriveError::Store)?;
            return self
                .complete_plan(run_id, tools, index, &plan, &menu, false)
                .await;
        }
        let request =
            RequestInput::from_tool_args(args.as_ref().clone()).map_err(tool_error_to_drive)?;
        let mut menu = request.menu(self.next_menu_id());
        let request_gate = if request.has_declared_default() {
            InteractionGate::RequestInputWithDefault
        } else {
            InteractionGate::RequestInputWithoutDefault
        };
        if self.config.interaction_policy.resolve(request_gate) != InteractionResolution::AwaitHuman
        {
            menu.blocking = false;
        }
        self.commit_payload(
            run_id,
            EventPayload::MenuOpened(menu.clone()),
            prompt_omit_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        self.resolve_or_wait_for_request_input(
            run_id,
            tools,
            index,
            cancel,
            RequestInputResolutionContext {
                request,
                menu,
                recovered_open_menu: false,
            },
        )
        .await
    }

    /// Journals the automatic settlement of a presented plan and returns its
    /// fixed accepted result without entering `InputRequired` or a wait loop.
    async fn complete_plan(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        plan: &Plan,
        menu: &Menu,
        answer_already_committed: bool,
    ) -> Result<Message, DriveError> {
        if !answer_already_committed {
            let answer = plan.automatic_answer(menu).map_err(tool_error_to_drive)?;
            self.commit_payload(
                run_id,
                EventPayload::MenuAnswered(answer),
                prompt_omit_render(),
            )
            .await
            .map_err(DriveError::Store)?;
        }
        let result = serde_json::json!(plan.accepted_result()).to_string();
        let call_id = tools[index].call_id.clone();
        let bounded = BoundedResult {
            preview: result.clone(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        };
        let bounded = tools[index].correct_result(bounded);
        self.commit_tool_settlement_and_streaming(run_id, &tools[index], &bounded)
            .await?;
        tools.remove(index);
        Ok(Message::tool_result(call_id, bounded.preview, false))
    }

    async fn resume_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
        menu: Menu,
    ) -> Result<Message, DriveError> {
        let args = parse_tool_args(&tools[index])?;
        let name = tools[index].name.clone();
        if name == "plan" {
            let plan = Plan::from_tool_args(args.as_ref().clone()).map_err(tool_error_to_drive)?;
            let answer_already_committed = self
                .committed_menus
                .borrow_and_update()
                .as_ref()
                .is_some_and(|envelope| {
                    envelope.payload.decode_event().is_ok_and(
                        |payload| {
                            matches!(payload, EventPayload::MenuAnswered(answer) if answer.menu == menu.id)
                        },
                    )
                });
            return self
                .complete_plan(run_id, tools, index, &plan, &menu, answer_already_committed)
                .await;
        }
        let request =
            RequestInput::from_tool_args(args.as_ref().clone()).map_err(tool_error_to_drive)?;
        self.resolve_or_wait_for_request_input(
            run_id,
            tools,
            index,
            cancel,
            RequestInputResolutionContext {
                request,
                menu,
                recovered_open_menu: true,
            },
        )
        .await
    }

    async fn resolve_or_wait_for_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
        context: RequestInputResolutionContext,
    ) -> Result<Message, DriveError> {
        let RequestInputResolutionContext {
            request,
            menu,
            recovered_open_menu,
        } = context;
        let gate = if request.has_declared_default() {
            InteractionGate::RequestInputWithDefault
        } else {
            InteractionGate::RequestInputWithoutDefault
        };
        match self.config.interaction_policy.resolve(gate) {
            InteractionResolution::AwaitHuman => {
                if !recovered_open_menu {
                    self.commit_state(
                        run_id,
                        RunState::InputRequired {
                            menu: menu.id.clone(),
                        },
                    )
                    .await
                    .map_err(DriveError::Store)?;
                }
                self.wait_for_request_input(run_id, tools, index, cancel, request, menu)
                    .await
            }
            InteractionResolution::UseDeclaredDefault => {
                let answer = request
                    .declared_default_answer(&menu)
                    .map_err(tool_error_to_drive)?;
                let resolved = request
                    .resolve(&menu, &answer)
                    .map_err(tool_error_to_drive)?;
                let answer_already_committed =
                    recovered_open_menu && self.automatic_menu_answer_already_committed(&answer)?;
                if !answer_already_committed {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuAnswered(answer),
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                }
                self.complete_resolved_request_input(run_id, tools, index, resolved)
                    .await
            }
            InteractionResolution::ReturnNoHumanAvailable => {
                self.complete_unanswered_autonomous_request_input(run_id, tools, index, &menu)
                    .await
            }
            _ => Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "request_input interaction policy returned an incompatible resolution",
                false,
            ))),
        }
    }

    async fn complete_resolved_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        resolved: haider_tools::RequestInputAnswer,
    ) -> Result<Message, DriveError> {
        let result = serde_json::json!({
            "value": resolved.value,
            "option_key": resolved.option_key,
        })
        .to_string();
        let call_id = tools[index].call_id.clone();
        let bounded = BoundedResult {
            preview: result.clone(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        };
        let bounded = tools[index].correct_result(bounded);
        self.commit_tool_settlement_and_streaming(run_id, &tools[index], &bounded)
            .await?;
        tools.remove(index);
        Ok(Message::tool_result(call_id, bounded.preview, false))
    }

    async fn complete_unanswered_autonomous_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        menu: &Menu,
    ) -> Result<Message, DriveError> {
        let reason = "no human is available and request_input declared no default".to_owned();
        let result = serde_json::json!({
            "ok": false,
            "code": "no_human_available",
            "message": &reason,
        })
        .to_string();
        let call_id = tools[index].call_id.clone();
        let bounded = tools[index].correct_result(BoundedResult {
            preview: result.clone(),
            truncated: false,
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: ToolResultStatus::Rejected,
            reason: Some(reason.clone()),
            presentation: Some(ErrorPresentation::new(
                "no_human_available",
                "No human available",
                &reason,
                ErrorScope::Tool,
                [ErrorAction::None],
            )),
        });
        self.commit_payload(
            run_id,
            EventPayload::ToolResult {
                call_id: call_id.clone(),
                result: bounded.clone(),
            },
            prompt_verbatim_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        self.commit_payload(
            run_id,
            EventPayload::MenuClosed {
                menu: menu.id.clone(),
                reason: MenuCloseReason::Dismissed,
            },
            prompt_omit_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        // `MenuClosed` is an externally consumed lifecycle boundary: the
        // rejected result must remain durable before it, while item closure and
        // resumed streaming can share the following append.
        self.commit_tool_completion_and_streaming(run_id, &tools[index], ToolStatus::Rejected)
            .await?;
        tools.remove(index);
        Ok(Message::tool_result(call_id, bounded.preview, false))
    }

    async fn resume_tool_approval(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
        checkpoint: RequestInputCheckpoint,
    ) -> Result<Message, DriveError> {
        let Some(dispatcher) = self.dispatcher.as_ref().map(Arc::clone) else {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "recovered permission checkpoint has no tool dispatcher",
                false,
            )));
        };
        dispatcher
            .activate_approval(run_id, &checkpoint)
            .await
            .map_err(DriveError::Store)?;
        let answer = self
            .wait_for_permission_answer(run_id, cancel, &checkpoint.menu)
            .await?;
        dispatcher
            .resolve_approval(&checkpoint.menu, &answer)
            .await
            .map_err(DriveError::Store)?;
        self.commit_state(run_id, RunState::RunningTool)
            .await
            .map_err(DriveError::Store)?;
        let args = parse_tool_args(&tools[index])?;
        let outcome = self
            .execute_general_tool(run_id, &tools[index], args, cancel, &dispatcher)
            .await?;
        let GeneralToolOutcome::Completed(result) = outcome else {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "a recovered approval unexpectedly became deferred",
                false,
            )));
        };
        let result = tools[index].correct_result(result);
        self.admit_tool_result_images(&result.images).await?;
        let call_id = tools[index].call_id.clone();
        self.commit_tool_settlement_and_streaming(run_id, &tools[index], &result)
            .await?;
        let projection = model_tool_result_projection(&tools[index].name, &result);
        tools.remove(index);
        Ok(Message::tool_result_with_images(
            call_id,
            projection.preview,
            projection.truncated,
            result.images,
        ))
    }

    async fn wait_for_error_recovery_answer(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        menu: &Menu,
    ) -> Result<ErrorAction, DriveError> {
        loop {
            let wake = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    return Err(DriveError::Cancelled);
                },
                changed = self.committed_menus.changed(),
                    if self.committed_menus.has_changed().is_ok() =>
                {
                    match changed {
                        Ok(()) => self
                            .committed_menus
                            .borrow_and_update()
                            .clone()
                            .map(MenuWake::Committed),
                        Err(_) => None,
                    }
                },
                command = self.commands.recv() => command.map(MenuWake::Command),
            };
            let Some(wake) = wake else {
                self.commit_payload(
                    run_id,
                    EventPayload::MenuClosed {
                        menu: menu.id.clone(),
                        reason: MenuCloseReason::Dismissed,
                    },
                    prompt_omit_render(),
                )
                .await
                .map_err(DriveError::Store)?;
                return Err(DriveError::Provider(provider_protocol_error(
                    "session actor command channel closed with recovery unanswered",
                )));
            };
            let (answer, completed, already_committed) = match wake {
                MenuWake::Command(command @ ActorCommand::Submit { .. }) => {
                    self.defer_submit_or_reject(command);
                    continue;
                }
                MenuWake::Command(
                    command @ (ActorCommand::Nudge { .. } | ActorCommand::PromotedSteerWake { .. }),
                ) => {
                    self.service_command_without_menu(command);
                    continue;
                }
                MenuWake::Command(ActorCommand::AnswerMenu { answer, completed }) => {
                    (answer, Some(completed), false)
                }
                MenuWake::Command(ActorCommand::Stop { completed }) => {
                    cancel.cancel();
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    let _ = completed.send(());
                    return Err(DriveError::Cancelled);
                }
                MenuWake::Committed(envelope) => {
                    let payload = envelope.payload.decode_event().map_err(|error| {
                        DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("committed recovery wake could not decode: {error}"),
                            false,
                        ))
                    })?;
                    let EventPayload::MenuAnswered(answer) = payload else {
                        return Err(DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "committed recovery wake did not contain MenuAnswered",
                            false,
                        )));
                    };
                    (answer, None, true)
                }
            };
            if answer.menu != menu.id {
                let error = HaiderError::new(
                    ErrorCode::MenuNotFound,
                    format!(
                        "menu {} is not open; recovery is waiting on {}",
                        answer.menu, menu.id
                    ),
                    false,
                );
                if let Some(completed) = completed {
                    let _ = completed.send(Err(error));
                    continue;
                }
                return Err(DriveError::Store(error));
            }
            let action = match selected_error_action(menu, &answer) {
                Ok(action) => action,
                Err(error) => {
                    if let Some(completed) = completed {
                        let _ = completed.send(Err(error));
                        continue;
                    }
                    return Err(DriveError::Store(error));
                }
            };
            if !already_committed
                && let Err(error) = self
                    .commit_payload(
                        run_id,
                        EventPayload::MenuAnswered(answer),
                        prompt_omit_render(),
                    )
                    .await
            {
                if let Some(completed) = completed {
                    let _ = completed.send(Err(error.clone()));
                }
                return Err(DriveError::Store(error));
            }
            if let Some(completed) = completed {
                let _ = completed.send(Ok(()));
            }
            return Ok(action);
        }
    }

    async fn resolve_partial_stream_recovery(
        &mut self,
        run_id: &RunId,
        cancel: &CancelToken,
        menu: &Menu,
        recovered_open_menu: bool,
    ) -> Result<ErrorAction, DriveError> {
        match self
            .config
            .interaction_policy
            .resolve(InteractionGate::PartialProviderStream)
        {
            InteractionResolution::ContinuePartial => {
                let answer = automatic_error_action_answer(menu, ErrorAction::ContinuePartial)
                    .map_err(DriveError::Store)?;
                let answer_already_committed =
                    recovered_open_menu && self.automatic_menu_answer_already_committed(&answer)?;
                if !answer_already_committed {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuAnswered(answer),
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                }
                Ok(ErrorAction::ContinuePartial)
            }
            InteractionResolution::AwaitHuman => {
                if !recovered_open_menu {
                    self.commit_state(
                        run_id,
                        RunState::InputRequired {
                            menu: menu.id.clone(),
                        },
                    )
                    .await
                    .map_err(DriveError::Store)?;
                }
                self.wait_for_error_recovery_answer(run_id, cancel, menu)
                    .await
            }
            _ => Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "partial-stream interaction policy returned an incompatible resolution",
                false,
            ))),
        }
    }

    fn automatic_menu_answer_already_committed(
        &mut self,
        expected: &MenuAnswer,
    ) -> Result<bool, DriveError> {
        let Some(envelope) = self.committed_menus.borrow_and_update().clone() else {
            return Ok(false);
        };
        let payload = envelope.payload.decode_event().map_err(|error| {
            DriveError::Store(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("recovered menu answer does not decode: {error}"),
                false,
            ))
        })?;
        let EventPayload::MenuAnswered(committed) = payload else {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "recovered menu wake is not a menu answer",
                false,
            )));
        };
        if &committed != expected {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "recovered automatic menu answer differs from the declared policy resolution",
                false,
            )));
        }
        Ok(true)
    }

    async fn wait_for_request_input(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
        request: RequestInput,
        menu: Menu,
    ) -> Result<Message, DriveError> {
        loop {
            let wake = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    return Err(DriveError::Cancelled);
                },
                // Guarded so a dropped wake channel cannot masquerade as
                // "menu closed" — only the command channel decides closure.
                // The watch's initial `None` never fires `changed`; a `Some`
                // here is always a really-committed answer.
                changed = self.committed_menus.changed(),
                    if self.committed_menus.has_changed().is_ok() =>
                {
                    match changed {
                        Ok(()) => self
                            .committed_menus
                            .borrow_and_update()
                            .clone()
                            .map(MenuWake::Committed),
                        Err(_) => None,
                    }
                },
                command = self.commands.recv() => command.map(MenuWake::Command),
            };
            let Some(wake) = wake else {
                self.commit_payload(
                    run_id,
                    EventPayload::MenuClosed {
                        menu: menu.id.clone(),
                        reason: MenuCloseReason::Dismissed,
                    },
                    prompt_omit_render(),
                )
                .await
                .map_err(DriveError::Store)?;
                return Err(DriveError::Provider(provider_protocol_error(
                    "session actor command channel closed with request_input unanswered",
                )));
            };
            let (answer, completed, already_committed) = match wake {
                MenuWake::Command(command @ ActorCommand::Submit { .. }) => {
                    self.defer_submit_or_reject(command);
                    continue;
                }
                MenuWake::Command(
                    command @ (ActorCommand::Nudge { .. } | ActorCommand::PromotedSteerWake { .. }),
                ) => {
                    self.service_command_without_menu(command);
                    continue;
                }
                MenuWake::Command(ActorCommand::AnswerMenu { answer, completed }) => {
                    (answer, Some(completed), false)
                }
                MenuWake::Command(ActorCommand::Stop { completed }) => {
                    cancel.cancel();
                    self.commit_payload(
                        run_id,
                        EventPayload::MenuClosed {
                            menu: menu.id.clone(),
                            reason: MenuCloseReason::Cancelled,
                        },
                        prompt_omit_render(),
                    )
                    .await
                    .map_err(DriveError::Store)?;
                    let _ = completed.send(());
                    return Err(DriveError::Cancelled);
                }
                MenuWake::Committed(envelope) => {
                    let payload = envelope.payload.decode_event().map_err(|error| {
                        DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("committed menu wake could not decode: {error}"),
                            false,
                        ))
                    })?;
                    let EventPayload::MenuAnswered(answer) = payload else {
                        return Err(DriveError::Store(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "committed menu wake did not contain MenuAnswered",
                            false,
                        )));
                    };
                    (answer, None, true)
                }
            };
            if answer.menu != menu.id {
                if let Some(completed) = completed {
                    let _ = completed.send(Err(HaiderError::new(
                        ErrorCode::MenuNotFound,
                        format!(
                            "menu {} is not open; request_input is waiting on {}",
                            answer.menu, menu.id
                        ),
                        false,
                    )));
                    continue;
                }
                return Err(DriveError::Store(HaiderError::new(
                    ErrorCode::MenuNotFound,
                    format!(
                        "committed answer for menu {} reached waiter for {}",
                        answer.menu, menu.id
                    ),
                    false,
                )));
            }
            let resolved = match request.resolve(&menu, &answer) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let error =
                        HaiderError::new(ErrorCode::InvalidArgument, error.to_string(), false);
                    if let Some(completed) = completed {
                        let _ = completed.send(Err(HaiderError::new(
                            error.code,
                            error.message,
                            error.retryable,
                        )));
                        continue;
                    }
                    return Err(DriveError::Store(error));
                }
            };
            if !already_committed
                && let Err(error) = self
                    .commit_payload(
                        run_id,
                        EventPayload::MenuAnswered(answer),
                        prompt_omit_render(),
                    )
                    .await
            {
                if let Some(completed) = completed {
                    let _ = completed.send(Err(error.clone()));
                }
                return Err(DriveError::Store(error));
            }
            let result = serde_json::json!({
                "value": resolved.value,
                "option_key": resolved.option_key,
            })
            .to_string();
            let call_id = tools[index].call_id.clone();
            let bounded = BoundedResult {
                preview: result.clone(),
                truncated: false,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            };
            let bounded = tools[index].correct_result(bounded);
            if let Err(error) = self
                .commit_tool_settlement_and_streaming(run_id, &tools[index], &bounded)
                .await
            {
                if let Some(completed) = completed
                    && let DriveError::Store(store_error) = &error
                {
                    let _ = completed.send(Err(store_error.clone()));
                }
                return Err(error);
            }
            tools.remove(index);
            if let Some(completed) = completed {
                let _ = completed.send(Ok(()));
            }
            return Ok(Message::tool_result(call_id, bounded.preview, false));
        }
    }

    async fn settle_deferred_tools(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        deferred: &mut Vec<DeferredAccumulator>,
        cancel: &CancelToken,
    ) -> Result<Vec<Message>, DriveError> {
        let Some(dispatcher) = self.dispatcher.as_ref().map(Arc::clone) else {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "deferred child wait has no tool dispatcher",
                false,
            )));
        };
        let mut results = Vec::with_capacity(deferred.len());
        while let Some(mut pending) = deferred.first().cloned() {
            let completion = loop {
                let collection = dispatcher.collect_deferred(&pending.ticket, cancel);
                let wake = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(DriveError::Cancelled),
                    result = collection => Some(result),
                    command = self.commands.recv() => {
                        let Some(command) = command else {
                            return Err(DriveError::Store(HaiderError::new(
                                ErrorCode::Internal,
                                "session actor command channel closed during child wait",
                                true,
                            )));
                        };
                        match command {
                            ActorCommand::Stop { completed } => {
                                cancel.cancel();
                                let _ = completed.send(());
                                return Err(DriveError::Cancelled);
                            }
                            other => self.service_command_without_menu(other),
                        }
                        None
                    }
                };
                if let Some(result) = wake {
                    break match result {
                        Ok(completion) => completion,
                        // Delegation owns a typed, run-deadline-derived wait
                        // bound. Do not turn that terminal condition into an
                        // ordinary red tool result and issue another provider
                        // request; preserve it as RunFailed + Errored.
                        Err(error) if delegated_child_wait_timed_out(&error) => {
                            return Err(DriveError::Store(error));
                        }
                        Err(error) => DeferredToolResult {
                            report: ChildReport {
                                agent: pending.ticket.manifest.agent.clone(),
                                summary: sanitized_failure_message(&error.message),
                                verified: ReportVerification::Red,
                                workspace_revision: None,
                            },
                            chip: ChipState::Error,
                            truncated: false,
                        },
                    };
                }
            };
            let result = BoundedResult {
                // The opaque id is operational routing, not display identity:
                // a later `message_subagent` call must be able to name the
                // direct child without guessing from task/callsign text.
                preview: format!(
                    "agent: {}\n\n{}",
                    completion.report.agent, completion.report.summary
                ),
                truncated: completion.truncated,
                data: None,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: if completion.chip == ChipState::Error {
                    ToolResultStatus::Failed
                } else {
                    ToolResultStatus::Completed
                },
                reason: (completion.chip == ChipState::Error)
                    .then(|| sanitized_failure_message(&completion.report.summary)),
                presentation: (completion.chip == ChipState::Error).then(|| {
                    tool_error_presentation(
                        "child-agent-failed",
                        "Child agent failed",
                        "The delegated child ended without a successful result.",
                    )
                }),
            };
            if !pending.report_emitted {
                self.commit_payload(
                    run_id,
                    EventPayload::AgentReport(completion.report.clone()),
                    prompt_omit_render(),
                )
                .await
                .map_err(DriveError::Store)?;
                pending.report_emitted = true;
            }
            self.commit_payload(
                run_id,
                EventPayload::AgentChipState {
                    agent: completion.report.agent.clone(),
                    chip: completion.chip,
                },
                prompt_omit_render(),
            )
            .await
            .map_err(DriveError::Store)?;
            if !pending.child_result_emitted {
                self.commit_closed_item(
                    run_id,
                    TurnItem::ChildResult {
                        report: completion.report.clone(),
                    },
                )
                .await?;
                pending.child_result_emitted = true;
            }
            let tool_index = tools
                .iter()
                .position(|tool| tool.call_id == pending.call_id)
                .ok_or_else(|| {
                    DriveError::Store(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("deferred tool {} is missing", pending.call_id),
                        false,
                    ))
                })?;
            let result = tools[tool_index].correct_result(result);
            if !pending.tool_result_emitted {
                self.commit_payload(
                    run_id,
                    EventPayload::ToolResult {
                        call_id: pending.call_id.clone(),
                        result: result.clone(),
                    },
                    prompt_verbatim_render(),
                )
                .await
                .map_err(DriveError::Store)?;
                pending.tool_result_emitted = true;
            }
            if !pending.item_completed {
                self.commit_tool_completion_with_output_savings(
                    run_id,
                    &tools[tool_index],
                    &result,
                    result.status.item_status(),
                )
                .await?;
                pending.item_completed = true;
            }
            let projection = model_tool_result_projection(&tools[tool_index].name, &result);
            tools.remove(tool_index);
            dispatcher
                .acknowledge_deferred(&pending.ticket)
                .await
                .map_err(DriveError::Store)?;
            results.push(Message::tool_result(
                pending.call_id,
                projection.preview,
                projection.truncated,
            ));
            deferred.remove(0);
        }
        Ok(results)
    }

    async fn commit_closed_item(
        &mut self,
        run_id: &RunId,
        item: TurnItem,
    ) -> Result<(), DriveError> {
        let item_id = self.next_item_id();
        let envelopes = [
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                prompt_verbatim_render(),
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
                prompt_verbatim_render(),
            )
            .map_err(DriveError::Store)?,
        ];
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        self.append_and_publish_owned(Vec::from(envelopes))
            .await
            .map_err(DriveError::Store)?;
        Ok(())
    }

    /// Persists a visible transcript item that must not be replayed to a
    /// provider. Model refusal text is durable UI history, not assistant text.
    async fn commit_closed_item_omitted(
        &mut self,
        run_id: &RunId,
        item: TurnItem,
    ) -> Result<(), DriveError> {
        let item_id = self.next_item_id();
        let envelopes = [
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
        ];
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        self.append_and_publish_owned(Vec::from(envelopes))
            .await
            .map_err(DriveError::Store)?;
        Ok(())
    }

    /// Closes tools that are not parked deferred calls.
    async fn complete_non_deferred_tools(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        deferred: &[DeferredAccumulator],
        status: ToolStatus,
    ) -> Result<(), DriveError> {
        while let Some(index) = tools.iter().position(|tool| {
            !deferred
                .iter()
                .any(|pending| pending.call_id == tool.call_id)
        }) {
            self.commit_tool_completed(run_id, &tools[index], status)
                .await?;
            tools.remove(index);
        }
        Ok(())
    }

    /// Settles every call opened by the interrupted provider response as a
    /// non-executed proposal before a Subturn request begins. Parallel calls
    /// may still have partial arguments when the first resolved call reaches
    /// the boundary, so this path preserves raw fragments instead of turning
    /// the user-requested hold into a provider-protocol failure.
    async fn complete_tools_for_subturn(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
    ) -> Result<(), DriveError> {
        while !tools.is_empty() {
            let tool = &tools[0];
            self.commit_item(
                run_id,
                ItemEvent::Completed {
                    item_id: tool.item_id.clone(),
                    item: TurnItem::ToolCall {
                        call_id: tool.call_id.clone(),
                        name: tool.name.clone(),
                        args: tool_args_or_raw(tool),
                        status: ToolStatus::Pending,
                    },
                },
            )
            .await
            .map_err(DriveError::Store)?;
            tools.remove(0);
        }
        Ok(())
    }

    /// Closes every tool still open, in start order.
    async fn complete_all_tools(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        status: ToolStatus,
    ) -> Result<(), DriveError> {
        while !tools.is_empty() {
            self.commit_tool_completed(run_id, &tools[0], status)
                .await?;
            tools.remove(0);
        }
        Ok(())
    }

    /// Commits `Completed`; failed terminal cleanup preserves malformed partial
    /// arguments as a JSON string rather than leaving the item dangling.
    async fn commit_tool_completed(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        status: ToolStatus,
    ) -> Result<(), DriveError> {
        // Terminal cleanup (Failed OR Cancelled) must never parse-fail: a
        // cancel with half-streamed args would otherwise convert into Errored,
        // violating cancellation-as-outcome. Preserve partial args as raw.
        let args = if matches!(status, ToolStatus::Failed | ToolStatus::Cancelled) {
            tool_args_or_raw(tool)
        } else {
            parse_tool_args(tool)?.as_ref().clone()
        };
        self.commit_item(
            run_id,
            ItemEvent::Completed {
                item_id: tool.item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                    args,
                    status,
                },
            },
        )
        .await
        .map_err(DriveError::Store)?;
        Ok(())
    }

    async fn commit_tool_result_and_completion(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        result: &BoundedResult,
    ) -> Result<(), DriveError> {
        self.commit_tool_settlement(
            run_id,
            tool,
            Some(result),
            Some(result),
            result.status.item_status(),
            false,
        )
        .await
    }

    /// Atomically publishes a finished tool's result, completed item/tree
    /// fragment, and resumed `Streaming` state. Any effectful caller retains a
    /// prior, independent `RunningTool` append before dispatch.
    async fn commit_tool_settlement_and_streaming(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        result: &BoundedResult,
    ) -> Result<(), DriveError> {
        self.commit_tool_settlement(
            run_id,
            tool,
            Some(result),
            Some(result),
            result.status.item_status(),
            true,
        )
        .await
    }

    async fn commit_tool_completion_and_streaming(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        status: ToolStatus,
    ) -> Result<(), DriveError> {
        self.commit_tool_settlement(run_id, tool, None, None, status, true)
            .await
    }

    /// Completes a deferred tool whose result was already journaled, while
    /// atomically attaching the one authoritative output-savings event.
    async fn commit_tool_completion_with_output_savings(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        result: &BoundedResult,
        status: ToolStatus,
    ) -> Result<(), DriveError> {
        self.commit_tool_settlement(run_id, tool, None, Some(result), status, false)
            .await
    }

    async fn commit_tool_settlement(
        &mut self,
        run_id: &RunId,
        tool: &ToolAccumulator,
        result: Option<&BoundedResult>,
        savings_source: Option<&BoundedResult>,
        status: ToolStatus,
        resume_streaming: bool,
    ) -> Result<(), DriveError> {
        let args = if matches!(status, ToolStatus::Failed | ToolStatus::Cancelled) {
            tool_args_or_raw(tool)
        } else {
            parse_tool_args(tool)?.as_ref().clone()
        };

        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        let result_envelope = result
            .map(|result| {
                self.uncommitted_envelope(
                    run_id,
                    EventPayload::ToolResult {
                        call_id: tool.call_id.clone(),
                        result: result.clone(),
                    },
                    prompt_verbatim_render(),
                )
            })
            .transpose()
            .map_err(DriveError::Store)?;
        let node = TreeNode {
            node: self.next_node_id(),
            parent: self.tree_parent().await.map_err(DriveError::Store)?,
            kind: NodeKind::ToolExchange {
                tool: tool.name.clone(),
                summary: format!("tool call settled as {status:?}"),
                artifact: None,
            },
        };
        let economy_update = savings_source
            .and_then(|result| model_tool_result_projection(&tool.name, result).savings)
            .map(|mut output| {
                output.source_item_id = Some(tool.item_id.to_string());
                self.config.context_economy.record_tool_output(output)
            });
        let savings_envelopes = economy_update
            .as_ref()
            .map(|(_, event)| {
                let item = event.extension_item().map_err(|error| {
                    DriveError::Store(HaiderError::new(
                        ErrorCode::Internal,
                        format!("output context savings could not serialize: {error}"),
                        false,
                    ))
                })?;
                let TurnItem::Extension { kind, data } = item else {
                    return Err(DriveError::Store(HaiderError::new(
                        ErrorCode::Internal,
                        "output context savings did not use the extension carrier",
                        false,
                    )));
                };
                self.uncommitted_extension_marker(run_id, &kind, data, prompt_omit_render())
                    .map_err(DriveError::Store)
            })
            .transpose()?;
        let mut envelopes = Vec::with_capacity(
            2 + usize::from(result.is_some())
                + usize::from(resume_streaming)
                + usize::from(savings_envelopes.is_some()) * 2,
        );
        if let Some(result) = result_envelope {
            envelopes.push(result);
        }
        envelopes.push(
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: tool.item_id.clone(),
                    item: TurnItem::ToolCall {
                        call_id: tool.call_id.clone(),
                        name: tool.name.clone(),
                        args,
                        status,
                    },
                }),
                prompt_verbatim_render(),
            )
            .map_err(DriveError::Store)?,
        );
        if let Some(savings_envelopes) = savings_envelopes {
            envelopes.extend(savings_envelopes);
        }
        envelopes.push(
            self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
        );
        if resume_streaming {
            envelopes.push(
                self.uncommitted_envelope(
                    run_id,
                    EventPayload::RunState(RunState::Streaming),
                    prompt_omit_render(),
                )
                .map_err(DriveError::Store)?,
            );
        }
        self.append_and_publish_owned(envelopes)
            .await
            .map_err(DriveError::Store)?;
        if let Some((economy, _)) = economy_update {
            self.config.context_economy = economy;
            self.store
                .persist_context_economy(&self.config.session_id, &self.config.context_economy)
                .await
                .map_err(DriveError::Store)?;
        }
        self.tree_head = Some(node.node);
        if resume_streaming {
            self.state.send_replace(Some(RunState::Streaming));
        }
        Ok(())
    }

    /// Maps the provider's finish reason onto the terminal run state.
    async fn finish_outcome(&mut self, run_id: &RunId, reason: FinishReason) -> TurnOutcome {
        match self.commit_state(run_id, RunState::Done).await {
            Ok(()) => TurnOutcome {
                state: RunState::Done,
                finish_reason: reason,
                error: None,
            },
            // A one-shot store failure while appending `Done` still gets an
            // honest terminal envelope on the next append.
            Err(error) => self.errored_state_outcome(run_id, error).await,
        }
    }

    async fn cancelled_outcome(&mut self, run_id: &RunId) -> TurnOutcome {
        if self.config.supervisor_commits_cancelled {
            self.state.send_replace(Some(RunState::Cancelled));
            return TurnOutcome {
                state: RunState::Cancelled,
                finish_reason: FinishReason::Cancelled,
                error: None,
            };
        }
        match self.commit_state(run_id, RunState::Cancelled).await {
            Ok(()) => TurnOutcome {
                state: RunState::Cancelled,
                finish_reason: FinishReason::Cancelled,
                error: None,
            },
            Err(error) => self.errored_state_outcome(run_id, error).await,
        }
    }

    async fn cancelled_outcome_with_items(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        tools: &mut Vec<ToolAccumulator>,
    ) -> TurnOutcome {
        let mut cleanup_error = None;
        if let Some(dispatcher) = self.dispatcher.as_ref() {
            if let Err(error) = dispatcher.cancel_outstanding_deferred().await {
                cleanup_error = Some(error);
            }
            // P1-1: a cancel/close failure cannot turn a committed user
            // cancellation into a failure — the daemon owner reconciles
            // every abandoned dispatch after the actor settles.
            let _ = dispatcher.cancel().await;
        }
        if let Err(error) = self
            .complete_open_items(run_id, message, reasoning, tools, ToolStatus::Cancelled)
            .await
        {
            return errored_outcome(drive_error_to_haider(error));
        }
        if let Some(error) = cleanup_error {
            return errored_outcome(error);
        }
        self.cancelled_outcome(run_id).await
    }

    async fn provider_failure_outcome_with_items(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        tools: &mut Vec<ToolAccumulator>,
        mut provider_error: ProviderError,
    ) -> TurnOutcome {
        // This is the single deadline-to-terminal classifier. The durable
        // state wins over whichever timer happened to wake the actor: expiry
        // during provider backoff/admission is bounded retry exhaustion, while
        // an active provider request/stream retains `provider_timeout`.
        let run_state = self.state.borrow().clone();
        let deadline_expired_during_retry = classify_provider_deadline_terminal(
            &mut provider_error,
            run_state.as_ref(),
            self.provider_deadline_state.retry_admission_in_progress(),
        );
        if !deadline_expired_during_retry
            && provider_error.timeout_reason == Some(ProviderTimeoutReason::DeadlineExhausted)
            && let Some(guard) = self.config.provider_deadline_guard.clone()
        {
            match guard.map_deadline_exhausted(run_id).await {
                Ok(Some(error)) | Err(error) => {
                    return self
                        .errored_outcome_with_items(run_id, message, reasoning, tools, error)
                        .await;
                }
                Ok(None) => {}
            }
        }
        specialize_provider_presentation(&self.config.usage_scope.auth_scope, &mut provider_error);
        if let Some(card) = recovery_card_kind(&provider_error.presentation) {
            let menu = recovery_menu(
                self.next_menu_id(),
                run_id,
                None,
                card,
                provider_error.presentation.clone(),
                Some(self.config.usage_scope.provider.clone()),
                self.config.usage_account.clone(),
                false,
            );
            if let Err(error) = self
                .commit_payload(run_id, EventPayload::MenuOpened(menu), prompt_omit_render())
                .await
            {
                return errored_outcome(error);
            }
        }
        self.errored_outcome_with_items(
            run_id,
            message,
            reasoning,
            tools,
            provider_error_to_haider(provider_error),
        )
        .await
    }

    async fn drive_error_outcome_with_items(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        tools: &mut Vec<ToolAccumulator>,
        error: DriveError,
    ) -> TurnOutcome {
        match error {
            DriveError::Provider(error) => {
                self.provider_failure_outcome_with_items(run_id, message, reasoning, tools, error)
                    .await
            }
            DriveError::Cancelled => {
                self.cancelled_outcome_with_items(run_id, message, reasoning, tools)
                    .await
            }
            other => {
                self.errored_outcome_with_items(
                    run_id,
                    message,
                    reasoning,
                    tools,
                    drive_error_to_haider(other),
                )
                .await
            }
        }
    }

    async fn errored_outcome_with_items(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        tools: &mut Vec<ToolAccumulator>,
        error: HaiderError,
    ) -> TurnOutcome {
        if let Err(cleanup_error) = self
            .complete_open_items(run_id, message, reasoning, tools, ToolStatus::Failed)
            .await
        {
            // Never commit a terminal run state after a failed item close:
            // preserving the lifecycle law takes priority over best effort.
            return errored_outcome(drive_error_to_haider(cleanup_error));
        }
        self.errored_state_outcome(run_id, error).await
    }

    /// `terminal` is the status stamped on still-open tools: `Failed` for
    /// error paths, `Cancelled` for cancellation — the frozen law forbids
    /// rendering a cancelled turn's tools as failures.
    async fn complete_open_items(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        tools: &mut Vec<ToolAccumulator>,
        terminal: ToolStatus,
    ) -> Result<(), DriveError> {
        self.complete_text(run_id, message, false).await?;
        self.complete_text(run_id, reasoning, true).await?;
        self.complete_all_tools(run_id, tools, terminal).await
    }

    /// Commits `Errored` (best effort) and reports the original error.
    async fn errored_state_outcome(&mut self, run_id: &RunId, error: HaiderError) -> TurnOutcome {
        if let Err(commit_error) = self.commit_terminal_error(run_id, &error).await {
            return errored_outcome(commit_error);
        }
        TurnOutcome {
            state: RunState::Errored,
            finish_reason: FinishReason::Error,
            error: Some(error),
        }
    }

    /// Atomically commits durable failure detail plus its terminal run state.
    ///
    /// ATOMIC FAILURE TERMINAL (R3): `RunFailed` immediately precedes
    /// `Errored` in one store append. Besides removing a redundant
    /// transaction/actor round trip, this prevents a crash between two
    /// appends from leaving durable failure detail without a terminal state.
    async fn commit_terminal_error(
        &mut self,
        run_id: &RunId,
        error: &HaiderError,
    ) -> Result<(), HaiderError> {
        let mut envelopes = Vec::new();
        // The hard checkpoint and the named terminal share the same append:
        // recovery never sees a hard-bound handle without its terminal state.
        if error.code == ErrorCode::RequestBudgetExceeded {
            let data = error.details.clone().ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "request budget terminal is missing its checkpoint",
                    false,
                )
            })?;
            envelopes.extend(self.uncommitted_extension_marker(
                run_id,
                PROVIDER_REQUEST_BUDGET_EXTENSION_KIND,
                data,
                prompt_verbatim_render(),
            )?);
        }
        envelopes.extend([
            self.uncommitted_envelope(
                run_id,
                EventPayload::RunFailed {
                    code: error.code,
                    message: sanitized_failure_message(&error.message),
                    retryable: error.retryable,
                    presentation: Some(presentation_for_haider_error(error)),
                },
                prompt_omit_render(),
            )?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::RunState(RunState::Errored),
                prompt_omit_render(),
            )?,
        ]);
        self.flush_pending_item_delta().await?;
        self.append_and_publish_owned(envelopes).await?;
        self.state.send_replace(Some(RunState::Errored));
        Ok(())
    }

    /// Commits the run-state envelope, then mirrors it to the state watch.
    async fn commit_state(&mut self, run_id: &RunId, state: RunState) -> Result<(), HaiderError> {
        self.commit_payload(
            run_id,
            EventPayload::RunState(state.clone()),
            prompt_omit_render(),
        )
        .await?;
        self.state.send_replace(Some(state));
        Ok(())
    }

    async fn commit_pending_thinking(
        &mut self,
        run_id: &RunId,
        pending: &mut bool,
    ) -> Result<(), HaiderError> {
        if !*pending {
            return Ok(());
        }
        *pending = false;
        self.commit_state(run_id, RunState::Thinking).await
    }

    async fn commit_item(&mut self, run_id: &RunId, item: ItemEvent) -> Result<(), HaiderError> {
        let item = match item {
            ItemEvent::Delta { item_id, delta } => {
                return self
                    .buffer_provider_item_delta(run_id, item_id, delta)
                    .await;
            }
            item => item,
        };
        let node_kind = match &item {
            ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            } => Some(NodeKind::AssistantCommit {
                text: text.clone(),
                verdict: VerifyVerdict::NotApplicable,
            }),
            ItemEvent::Completed {
                item: TurnItem::ToolCall { name, status, .. },
                ..
            } => Some(NodeKind::ToolExchange {
                tool: name.clone(),
                summary: format!("tool call settled as {status:?}"),
                artifact: None,
            }),
            // G1: a FINISHED plan (non-empty, every item completed) commits
            // the durable Todos node — the "unpins into history" law. An
            // empty list is a clear, not a completed plan: no node.
            ItemEvent::Completed {
                item: TurnItem::Plan { items },
                ..
            } if !items.is_empty()
                && items.iter().all(|todo| todo.state == TodoState::Completed) =>
            {
                Some(NodeKind::Todos {
                    items: items.clone(),
                })
            }
            _ => None,
        };
        if let Some(node_kind) = node_kind {
            self.commit_tree_fragment(
                run_id,
                EventPayload::Item(item),
                prompt_verbatim_render(),
                node_kind,
            )
            .await?;
        } else {
            self.commit_payload(run_id, EventPayload::Item(item), prompt_verbatim_render())
                .await?;
        }
        Ok(())
    }

    /// Atomically journals a hidden provider-native block as one closed item.
    ///
    /// A single append keeps the item-lifecycle and worker-seal laws intact:
    /// no store failure can expose `Started` without its matching `Completed`.
    async fn commit_provider_opaque(
        &mut self,
        run_id: &RunId,
        block: &Block,
    ) -> Result<(), DriveError> {
        let Block::ProviderOpaque { provider, data } = block else {
            return Err(DriveError::Provider(provider_protocol_error(
                "provider-opaque commit received a non-opaque block",
            )));
        };
        let item_id = self.next_item_id();
        let item = TurnItem::Extension {
            kind: PROVIDER_OPAQUE_EXTENSION_KIND.into(),
            data: serde_json::json!({
                "provider": provider,
                "data": data.template(),
            }),
        };
        let render = hidden_prompt_verbatim_render();
        let parent = self.tree_parent().await.map_err(DriveError::Store)?;
        let node = TreeNode {
            node: self.next_node_id(),
            parent,
            kind: NodeKind::AssistantCommit {
                text: String::new().into(),
                verdict: VerifyVerdict::NotApplicable,
            },
        };
        let mut started = self
            .uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                render,
            )
            .map_err(DriveError::Store)?;
        let mut completed = self
            .uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
                render,
            )
            .map_err(DriveError::Store)?;
        if let Some(text) = data.reply_text()
            && (!started.payload.bind_provider_opaque_reply(text.clone())
                || !completed.payload.bind_provider_opaque_reply(text.clone()))
        {
            return Err(DriveError::Provider(provider_protocol_error(
                "provider-opaque reply template has no recognized native text field",
            )));
        }
        let envelopes = [
            started,
            completed,
            self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
        ];
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        self.append_and_publish_owned(Vec::from(envelopes))
            .await
            .map_err(DriveError::Store)?;
        self.tree_head = Some(node.node);
        Ok(())
    }

    /// Atomically journals one PROVIDER-executed tool call as a closed,
    /// UI-visible row followed by its bounded result (W-B decision 6).
    ///
    /// The render is prompt-OMIT on purpose: server tool state replays
    /// through the provider-opaque channel, and rendering this row into a
    /// later prompt would fabricate a client `tool_use` block with no paired
    /// result — a live 400.
    async fn commit_server_tool_row(
        &mut self,
        run_id: &RunId,
        call_id: &str,
        name: String,
        args: serde_json::Value,
        status: ToolStatus,
        result: &BoundedResult,
    ) -> Result<(), DriveError> {
        let item_id = self.next_item_id();
        let started = TurnItem::ToolCall {
            call_id: call_id.to_owned(),
            name: name.clone(),
            args: args.clone(),
            status: ToolStatus::InProgress,
        };
        let completed = TurnItem::ToolCall {
            call_id: call_id.to_owned(),
            name,
            args,
            status,
        };
        let render = prompt_omit_render();
        let started = self
            .uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: started,
                }),
                render,
            )
            .map_err(DriveError::Store)?;
        let completed = self
            .uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed {
                    item_id,
                    item: completed,
                }),
                render,
            )
            .map_err(DriveError::Store)?;
        self.flush_pending_item_delta()
            .await
            .map_err(DriveError::Store)?;
        let result = self
            .uncommitted_envelope(
                run_id,
                EventPayload::ToolResult {
                    call_id: call_id.to_owned(),
                    result: result.clone(),
                },
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?;
        self.append_and_publish_owned(vec![started, completed, result])
            .await
            .map_err(DriveError::Store)?;
        Ok(())
    }

    fn request_budget(&self) -> RequestBudgetV1 {
        RequestBudgetV1 {
            // Preserve embedders that only override the old hard-cap field.
            tranche: self
                .config
                .provider_request_tranche
                .min(self.config.max_provider_requests_per_turn),
            hard_cap: self.config.max_provider_requests_per_turn,
        }
    }

    fn request_budget_status(
        &self,
        run_id: &RunId,
        used: usize,
        phase: RequestBudgetPhaseV1,
    ) -> RequestBudgetStatusV1 {
        RequestBudgetStatusV1 {
            used,
            budget: self.request_budget(),
            phase,
            continuation: RequestBudgetContinuationV1 {
                session_id: self.config.session_id.clone(),
                run_id: run_id.clone(),
                branch_id: self.config.branch_id.clone(),
                agent_id: self.config.agent_id.clone(),
            },
        }
    }

    async fn commit_request_budget_note(
        &mut self,
        run_id: &RunId,
        status: &RequestBudgetStatusV1,
    ) -> Result<(), HaiderError> {
        let data = serde_json::to_value(status).map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("cannot encode request budget: {error}"),
                false,
            )
        })?;
        self.commit_extension_marker(
            run_id,
            PROVIDER_REQUEST_BUDGET_EXTENSION_KIND,
            data,
            prompt_verbatim_render(),
        )
        .await
    }

    async fn restore_request_budget(
        &self,
        run_id: &RunId,
    ) -> Result<(usize, Option<String>), HaiderError> {
        let mut cursor = 0;
        let mut used = 0;
        let mut note = None;
        loop {
            let page = self
                .store
                .read(&self.config.session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok((used, note));
            }
            for envelope in &page {
                if envelope.run_id.as_ref() == Some(run_id)
                    && let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                        envelope.payload.decode_event()
                    && let Some(status) = RequestBudgetStatusV1::from_extension_item(&item)
                {
                    used = used.max(status.used);
                    if status.phase == RequestBudgetPhaseV1::SoftBound {
                        note = Some(status.model_note());
                    }
                }
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        }
    }

    async fn commit_hidden_extension_marker(
        &mut self,
        run_id: &RunId,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<(), HaiderError> {
        self.commit_extension_marker(run_id, kind, data, hidden_prompt_omit_render())
            .await
    }

    /// Atomically publishes the exact provider-view marker, the request's
    /// pending `Thinking` transition, and its cache-attempt marker. Their
    /// journal order matches the former adjacent appends, but a crash can no
    /// longer expose the provider-view marker without the attempt facts.
    async fn commit_request_attempt(
        &mut self,
        run_id: &RunId,
        provider_request_ordinal: u64,
        provider_view: Option<(ProviderViewLedgerV1, Vec<ProviderViewBlobV1>)>,
        markers: RequestAttemptMarkers,
        thinking_pending: &mut bool,
    ) -> Result<Option<ProviderViewLedgerV1>, HaiderError> {
        let RequestAttemptMarkers {
            provider_view: provider_view_data,
            cache: cache_attempt_data,
            response_epoch,
            request_budget,
        } = markers;
        self.flush_pending_item_delta().await?;
        let mut envelopes = Vec::with_capacity(
            usize::from(provider_view_data.is_some()) * 2 + usize::from(*thinking_pending) + 4,
        );
        if let Some(data) = provider_view_data {
            envelopes.extend(self.uncommitted_extension_marker(
                run_id,
                PROVIDER_VIEW_ATTEMPT_EXTENSION_KIND,
                data,
                hidden_prompt_omit_render(),
            )?);
        }
        let thinking_index = if *thinking_pending {
            let index = envelopes.len();
            envelopes.push(self.uncommitted_envelope(
                run_id,
                EventPayload::RunState(RunState::Thinking),
                prompt_omit_render(),
            )?);
            Some(index)
        } else {
            None
        };
        envelopes.extend(self.uncommitted_extension_marker(
            run_id,
            CACHE_REQUEST_ATTEMPT_EXTENSION_KIND,
            cache_attempt_data,
            hidden_prompt_omit_render(),
        )?);
        if response_epoch != 0 {
            envelopes.extend(self.uncommitted_extension_marker(
                run_id,
                ROUTE_REPLAY_ATTEMPT_EXTENSION_KIND,
                serde_json::json!({ "response_epoch": response_epoch }),
                hidden_prompt_omit_render(),
            )?);
        }
        if let Some(status) = request_budget {
            envelopes.extend(self.uncommitted_extension_marker(
                run_id,
                PROVIDER_REQUEST_BUDGET_EXTENSION_KIND,
                serde_json::to_value(status).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("cannot encode request budget: {error}"),
                        false,
                    )
                })?,
                prompt_omit_render(),
            )?);
        }
        let (persisted_provider_view, committed) = match provider_view {
            Some((ledger, blobs)) => {
                let outcome = self
                    .store
                    .persist_provider_view_and_append_owned(ProviderViewAppendRequest {
                        session_id: self.config.session_id.clone(),
                        ledger,
                        blobs,
                        attempt_ordinal: provider_request_ordinal,
                        envelopes,
                    })
                    .await?;
                (Some(outcome.ledger), outcome.envelopes)
            }
            None => (None, self.store.append_owned(envelopes).await?),
        };
        for (index, envelope) in committed.iter().enumerate() {
            if self.events.receiver_count() != 0 {
                let _ = self.events.send(envelope.clone());
            }
            if thinking_index == Some(index) {
                self.state.send_replace(Some(RunState::Thinking));
            }
        }
        let _ = self.committed_batches.send(committed);
        *thinking_pending = false;
        Ok(persisted_provider_view)
    }

    fn uncommitted_extension_marker(
        &mut self,
        run_id: &RunId,
        kind: &str,
        data: serde_json::Value,
        render: RenderTargets,
    ) -> Result<[RawEnvelope; 2], HaiderError> {
        let item_id = self.next_item_id();
        let item = TurnItem::Extension {
            kind: kind.to_owned(),
            data,
        };
        Ok([
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                render,
            )?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
                render,
            )?,
        ])
    }

    async fn commit_ui_extension_marker(
        &mut self,
        run_id: &RunId,
        kind: &str,
        data: serde_json::Value,
    ) -> Result<(), HaiderError> {
        self.commit_extension_marker(run_id, kind, data, prompt_omit_render())
            .await
    }

    async fn commit_context_footprint(
        &mut self,
        run_id: &RunId,
        footprint: &ContextFootprint,
    ) -> Result<(), HaiderError> {
        let (kind, data) = context_footprint_extension(footprint)?;
        self.commit_ui_extension_marker(run_id, &kind, data).await
    }

    async fn commit_context_savings(
        &mut self,
        run_id: &RunId,
        tier: ContextCompactionTier,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        removed_tool_call_ids: Vec<String>,
    ) -> Result<ContextSavingsEvent, HaiderError> {
        let (economy, event) = self.config.context_economy.record_with_removed_tool_calls(
            tier,
            estimated_tokens_before,
            estimated_tokens_after,
            removed_tool_call_ids,
        );
        let item = event.extension_item().map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("context savings could not serialize: {error}"),
                false,
            )
        })?;
        let TurnItem::Extension { kind, data } = item else {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "context savings did not use the extension carrier",
                false,
            ));
        };
        self.commit_ui_extension_marker(run_id, &kind, data).await?;
        self.config.context_economy = economy;
        self.store
            .persist_context_economy(&self.config.session_id, &self.config.context_economy)
            .await?;
        Ok(event)
    }

    /// Commits the exact context measurement immediately before the usage fact
    /// in one append. Consumers observe the same envelope order, while a store
    /// failure exposes all three facts or none of them.
    async fn commit_usage_with_footprint(
        &mut self,
        run_id: &RunId,
        footprint: Option<&ContextFootprint>,
        usage: Usage,
    ) -> Result<(), HaiderError> {
        self.flush_pending_item_delta().await?;
        let envelopes = self.uncommitted_usage_envelopes(run_id, footprint, usage)?;
        self.append_and_publish_owned(envelopes).await?;
        Ok(())
    }

    fn uncommitted_usage_envelopes(
        &mut self,
        run_id: &RunId,
        footprint: Option<&ContextFootprint>,
        usage: Usage,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let mut envelopes = Vec::with_capacity(1 + usize::from(footprint.is_some()) * 2);
        if let Some(footprint) = footprint {
            let (kind, data) = context_footprint_extension(footprint)?;
            envelopes.extend(self.uncommitted_extension_marker(
                run_id,
                &kind,
                data,
                prompt_omit_render(),
            )?);
        }
        envelopes.push(self.uncommitted_envelope(
            run_id,
            EventPayload::Usage(usage),
            prompt_omit_render(),
        )?);
        Ok(envelopes)
    }

    async fn commit_pending_usage(
        &mut self,
        run_id: &RunId,
        pending: &mut Option<PendingUsageCommit>,
    ) -> Result<(), HaiderError> {
        let Some(pending) = pending.take() else {
            return Ok(());
        };
        self.commit_usage_with_footprint(run_id, pending.footprint.as_ref(), pending.usage)
            .await
    }

    /// Commits the no-boundary post-stream suffix in one append. Usage stays
    /// before item completion because that is the existing sequence/publish
    /// order; `Done` remains last. A crash before this append can therefore
    /// leave the run at `Started`/`Streaming` without `Done`, which the
    /// interrupted-turn recovery path already terminalizes. After success all
    /// facts are durable before any of them are published.
    async fn commit_post_stream_facts(
        &mut self,
        run_id: &RunId,
        message: &mut Option<TextAccumulator>,
        reasoning: &mut Option<TextAccumulator>,
        pending_usage: &mut Option<PendingUsageCommit>,
    ) -> Result<(), HaiderError> {
        if self.pending_item_delta.is_some() {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "post-stream batch crossed an unflushed item delta",
                false,
            ));
        }
        let usage_commit = pending_usage.as_ref().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "post-stream batch has no usage fact",
                false,
            )
        })?;
        if message.is_some() && !self.tree_head_initialized {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "post-stream batch has no initialized tree parent",
                false,
            ));
        }
        let message_parent = self.tree_head.clone();
        let mut envelopes = self.uncommitted_usage_envelopes(
            run_id,
            usage_commit.footprint.as_ref(),
            usage_commit.usage.clone(),
        )?;

        let message_node = if let Some(active) = message.as_mut() {
            let text = active.seal();
            let node = TreeNode {
                node: self.next_node_id(),
                parent: message_parent,
                kind: NodeKind::AssistantCommit {
                    text: text.clone(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            };
            envelopes.push(self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: active.item_id.clone(),
                    item: TurnItem::AgentMessage { text },
                }),
                prompt_verbatim_render(),
            )?);
            envelopes.push(self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )?);
            Some(node)
        } else {
            None
        };
        if let Some(active) = reasoning.as_mut() {
            let summary = active.seal();
            envelopes.push(self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: active.item_id.clone(),
                    item: TurnItem::Reasoning { summary },
                }),
                prompt_verbatim_render(),
            )?);
        }
        envelopes.push(self.uncommitted_envelope(
            run_id,
            EventPayload::RunState(RunState::Done),
            prompt_omit_render(),
        )?);

        self.append_and_publish_owned(envelopes).await?;
        if let Some(node) = message_node {
            self.tree_head = Some(node.node);
        }
        *message = None;
        *reasoning = None;
        *pending_usage = None;
        self.state.send_replace(Some(RunState::Done));
        Ok(())
    }

    async fn commit_extension_marker(
        &mut self,
        run_id: &RunId,
        kind: &str,
        data: serde_json::Value,
        render: RenderTargets,
    ) -> Result<(), HaiderError> {
        let envelopes = self.uncommitted_extension_marker(run_id, kind, data, render)?;
        self.flush_pending_item_delta().await?;
        self.append_and_publish_owned(Vec::from(envelopes)).await?;
        Ok(())
    }

    /// Atomically closes one exact journal fragment with the immutable tree
    /// node that selects it. A crash exposes both or neither to compilation.
    async fn commit_tree_fragment(
        &mut self,
        run_id: &RunId,
        payload: EventPayload,
        render: RenderTargets,
        kind: NodeKind,
    ) -> Result<RawEnvelope, HaiderError> {
        self.flush_pending_item_delta().await?;
        let node = TreeNode {
            node: self.next_node_id(),
            parent: self.tree_parent().await?,
            kind,
        };
        let envelopes = [
            self.uncommitted_envelope(run_id, payload, render)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )?,
        ];
        let committed = self.append_and_publish_owned(Vec::from(envelopes)).await?;
        self.tree_head = Some(node.node);
        committed.first().cloned().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "tree fragment append returned an empty committed batch",
                false,
            )
        })
    }

    async fn tree_parent(&mut self) -> Result<Option<NodeId>, HaiderError> {
        if !self.tree_head_initialized {
            self.tree_head = PromptHistoryCompiler::latest_head(
                self.store.as_ref(),
                &self.config.session_id,
                self.config.branch_id.as_ref(),
                self.config.agent_id.as_ref(),
            )
            .await?;
            self.tree_head_initialized = true;
        }
        Ok(self.tree_head.clone())
    }

    /// Stamps identity/fencing fields, appends (the store assigns `seq` and
    /// `committed_at_ms`), then broadcasts the committed envelope.
    async fn commit_payload(
        &mut self,
        run_id: &RunId,
        payload: EventPayload,
        render: RenderTargets,
    ) -> Result<RawEnvelope, HaiderError> {
        self.flush_pending_item_delta().await?;
        let envelopes = vec![self.uncommitted_envelope(run_id, payload, render)?];
        let committed = self.append_and_publish_owned(envelopes).await?;
        committed.first().cloned().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "single-envelope append returned an empty committed batch",
                false,
            )
        })
    }

    /// Holds only provider-stream deltas. A buffered value has not crossed the
    /// store append boundary, so cancellation or a process crash may lose it;
    /// every `Completed` or other semantic event flushes it first.
    async fn buffer_provider_item_delta(
        &mut self,
        run_id: &RunId,
        item_id: ItemId,
        delta: ItemDelta,
    ) -> Result<(), HaiderError> {
        if self
            .pending_item_delta_deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            self.flush_pending_item_delta().await?;
        }

        if let Some(pending) = self.pending_item_delta.as_mut()
            && pending.run_id == *run_id
            && pending.item_id == item_id
            && merge_contiguous_item_delta(&mut pending.delta, &delta)
        {
            return Ok(());
        }

        // An interleaved item or delta kind is a semantic ordering boundary:
        // commit the prior delta before accepting the next one.
        self.flush_pending_item_delta().await?;
        self.pending_item_delta = Some(PendingItemDelta {
            run_id: run_id.clone(),
            item_id,
            delta,
        });
        self.pending_item_delta_deadline =
            Some(tokio::time::Instant::now() + self.config.stream_delta_coalesce_window);
        Ok(())
    }

    /// Durably commits and publishes the one pending provider delta. Every
    /// durable non-delta emitter calls this barrier before its own append, and
    /// the stream select calls it at the configured deadline.
    async fn flush_pending_item_delta(&mut self) -> Result<(), HaiderError> {
        let Some(pending) = self.pending_item_delta.take() else {
            self.pending_item_delta_deadline = None;
            return Ok(());
        };
        self.pending_item_delta_deadline = None;

        let envelope = self.uncommitted_envelope(
            &pending.run_id,
            EventPayload::Item(ItemEvent::Delta {
                item_id: pending.item_id.clone(),
                delta: pending.delta.clone(),
            }),
            prompt_verbatim_render(),
        );
        let envelope = match envelope {
            Ok(envelope) => envelope,
            Err(error) => {
                self.restore_pending_item_delta(pending);
                return Err(error);
            }
        };
        if let Err(error) = self.append_and_publish_owned(vec![envelope]).await {
            self.restore_pending_item_delta(pending);
            return Err(error);
        }
        Ok(())
    }

    async fn append_and_publish_owned(
        &self,
        envelopes: Vec<RawEnvelope>,
    ) -> Result<Arc<[RawEnvelope]>, HaiderError> {
        let committed = self.store.append_owned(envelopes).await?;
        let fanout_started = self
            .config
            .turn_trace
            .as_ref()
            .map(TurnTraceContext::now_us_from_accept);
        if self.events.receiver_count() != 0 {
            for envelope in committed.iter() {
                let _ = self.events.send(envelope.clone());
            }
        }
        let _ = self.committed_batches.send(Arc::clone(&committed));
        if let (Some(trace), Some(started)) = (&self.config.turn_trace, fanout_started) {
            trace.emit(
                "core_event_fanout",
                0,
                0,
                started,
                trace.now_us_from_accept(),
            );
        }
        Ok(committed)
    }

    fn restore_pending_item_delta(&mut self, pending: PendingItemDelta) {
        debug_assert!(self.pending_item_delta.is_none());
        self.pending_item_delta = Some(pending);
        // A failed append must be retried before any later boundary can pass.
        self.pending_item_delta_deadline = Some(tokio::time::Instant::now());
    }

    fn uncommitted_envelope(
        &self,
        run_id: &RunId,
        mut payload: EventPayload,
        render: RenderTargets,
    ) -> Result<RawEnvelope, HaiderError> {
        if let EventPayload::ToolResult { result, .. } = &mut payload {
            ensure_tool_result_presentation(result);
        }
        let payload =
            haider_protocol::envelope::RawPayload::from_event(payload).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("event payload could not serialize: {error}"),
                    false,
                )
            })?;
        Ok(EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.next_event_id(),
            seq: 0,
            session_id: self.config.session_id.clone(),
            branch_id: self.config.branch_id.clone(),
            run_id: Some(run_id.clone()),
            agent_id: self.config.agent_id.clone(),
            device_id: self.config.device_id.clone(),
            authority_epoch: self.config.authority_epoch,
            worker_generation: self.config.worker_generation,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render,
            payload,
        })
    }

    fn next_node_id(&mut self) -> NodeId {
        self.next_node = self.next_node.saturating_add(1);
        NodeId::new(format!(
            "node-{}-{}-{}-{}",
            self.config.session_id,
            self.config.worker_generation,
            self.started_at_ms,
            self.next_node
        ))
    }

    fn next_run_id(&mut self) -> RunId {
        self.next_run += 1;
        RunId::new(format!(
            "run-{}-{}-{}-{}",
            self.config.session_id.as_str(),
            self.config.worker_generation,
            self.started_at_ms,
            self.next_run
        ))
    }

    fn next_event_id(&self) -> EventId {
        self.event_ids.next()
    }

    fn next_item_id(&mut self) -> ItemId {
        self.next_item += 1;
        ItemId::new(format!(
            "item-{}-{}-{}-{}",
            self.config.session_id.as_str(),
            self.config.worker_generation,
            self.started_at_ms,
            self.next_item
        ))
    }

    fn next_menu_id(&mut self) -> MenuId {
        self.next_menu += 1;
        MenuId::new(format!(
            "input-{}-{}-{}-{}",
            self.config.session_id.as_str(),
            self.config.worker_generation,
            self.started_at_ms,
            self.next_menu
        ))
    }

    async fn next_command(&mut self) -> Option<ActorCommand> {
        match self.deferred_commands.pop_front() {
            Some(command) => Some(command),
            None => self.commands.recv().await,
        }
    }

    fn service_command_without_menu(&mut self, command: ActorCommand) {
        match command {
            command @ ActorCommand::Submit { .. } => self.defer_submit_or_reject(command),
            ActorCommand::Nudge {
                text,
                mode: DeliveryMode::Steer,
            } => self.pending_nudges.push(text),
            ActorCommand::Nudge {
                text,
                mode: DeliveryMode::Subturn,
            } => self.pending_subturns.push(text),
            ActorCommand::Nudge {
                mode: DeliveryMode::Queue,
                ..
            } => {
                unreachable!("queue-mode input is admitted as a later logical turn")
            }
            ActorCommand::PromotedSteerWake { reservation_id } => {
                if let Some(text) = self.promoted_steers.take_committed(reservation_id) {
                    self.pending_nudges.push(text);
                }
            }
            ActorCommand::AnswerMenu { completed, .. } => {
                let _ = completed.send(Err(HaiderError::new(
                    ErrorCode::MenuNotFound,
                    "there is no open input menu",
                    false,
                )));
            }
            ActorCommand::Stop { .. } => {
                unreachable!("active-turn stop commands are handled before ordinary service")
            }
        }
    }

    fn defer_submit_or_reject(&mut self, command: ActorCommand) {
        if self.deferred_commands.len() >= self.config.deferred_command_capacity {
            let ActorCommand::Submit { accepted, .. } = command else {
                unreachable!("only Submit commands may be deferred");
            };
            let _ = accepted.send(Err(submit_busy_error(
                self.config.deferred_command_capacity,
            )));
        } else {
            self.deferred_commands.push_back(command);
        }
    }
}

fn raw_envelope_is_terminal(envelope: &RawEnvelope) -> bool {
    envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        == Some("run_state")
        && matches!(
            envelope
                .payload
                .get("state")
                .and_then(serde_json::Value::as_str),
            Some("done" | "errored" | "cancelled")
        )
}

fn context_footprint_extension(
    footprint: &ContextFootprint,
) -> Result<(String, serde_json::Value), HaiderError> {
    let item = footprint.extension_item().map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("context footprint could not serialize: {error}"),
            false,
        )
    })?;
    match item {
        TurnItem::Extension { kind, data } => Ok((kind, data)),
        _ => Err(HaiderError::new(
            ErrorCode::Internal,
            "context footprint did not use the extension carrier",
            false,
        )),
    }
}

async fn delta_flush_timer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Polls one provider event without ever yielding the actor task. Receiver
/// reads are cancel-safe, so dropping the pending future leaves the item for
/// the ordinary `recv` path. This is the exact adjacency test used by the
/// post-stream batching seam: `None` means an await would be required, while
/// `Some(None)` is an already-observed EOF.
fn poll_provider_stream_now(stream: &mut ProviderStream) -> Option<Option<ProviderStreamItem>> {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let recv = stream.recv();
    let mut recv = std::pin::pin!(recv);
    match std::future::Future::poll(recv.as_mut(), &mut context) {
        std::task::Poll::Ready(item) => Some(item),
        std::task::Poll::Pending => None,
    }
}

fn merge_contiguous_item_delta(existing: &mut ItemDelta, incoming: &ItemDelta) -> bool {
    match (existing, incoming) {
        (ItemDelta::Text { text: accumulated }, ItemDelta::Text { text }) => {
            if let Some(joined) = accumulated.try_join(text) {
                *accumulated = joined;
            } else {
                let mut writer = ReplyArenaWriter::new().with_standard_provider_json_views();
                let _ = writer.append_shared(accumulated);
                let _ = writer.append_shared(text);
                *accumulated = writer.seal();
            }
            true
        }
        (ItemDelta::Reasoning { text: accumulated }, ItemDelta::Reasoning { text }) => {
            if let Some(joined) = accumulated.try_join(text) {
                *accumulated = joined;
            } else {
                let mut writer = ReplyArenaWriter::new();
                let _ = writer.append_shared(accumulated);
                let _ = writer.append_shared(text);
                *accumulated = writer.seal();
            }
            true
        }
        (
            ItemDelta::ToolArgs {
                fragment: accumulated,
            },
            ItemDelta::ToolArgs { fragment },
        ) => {
            accumulated.push_str(fragment);
            true
        }
        // Command output is raw base64 bytes, emitted by the daemon tool
        // sink rather than this actor; it deliberately remains uncoalesced.
        _ => false,
    }
}

/// Turn-loop failure, tagged by which port failed (drives the error surface).
#[derive(Debug)]
enum DriveError {
    Provider(ProviderError),
    Account(HaiderError),
    Store(HaiderError),
    Cancelled,
}

impl From<HaiderError> for DriveError {
    fn from(error: HaiderError) -> Self {
        Self::Store(error)
    }
}

impl From<ProviderBudgetGuardError> for DriveError {
    fn from(error: ProviderBudgetGuardError) -> Self {
        match error {
            ProviderBudgetGuardError::Cancelled => Self::Cancelled,
            ProviderBudgetGuardError::Failure(error) => Self::Store(error),
        }
    }
}

impl From<ContextCompactionError> for DriveError {
    fn from(error: ContextCompactionError) -> Self {
        match error {
            ContextCompactionError::Cancelled => Self::Cancelled,
            ContextCompactionError::Failure(error) => Self::Store(error),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProviderRetryContext<'a> {
    run_id: &'a RunId,
    cancel: &'a CancelToken,
}

/// One in-flight text or reasoning item (started, not yet completed).
#[derive(Debug)]
struct TextAccumulator {
    item_id: ItemId,
    provider_text: bool,
    writer: Option<ReplyArenaWriter>,
    external: Option<ReplyText>,
    sealed: Option<ReplyText>,
}

/// Keeps one logical assistant text block while the provider grows it through
/// multiple deltas. Both the prefix replacement and adjacent-range cases are
/// metadata-only operations over the shared reply arena.
fn append_assistant_text_block(blocks: &mut Vec<Block>, text: ReplyText) {
    if let Some(Block::Text { text: previous }) = blocks.last_mut() {
        let previous_range = previous.byte_range();
        let text_range = text.byte_range();
        if previous.shares_arena_with(&text)
            && previous_range.start == text_range.start
            && previous.is_prefix_of(&text)
        {
            *previous = text;
            return;
        }
        if let Some(joined) = previous.try_join(&text) {
            *previous = joined;
            return;
        }
    }
    blocks.push(Block::Text { text });
}

impl TextAccumulator {
    fn new(item_id: ItemId, provider_text: bool) -> Self {
        Self {
            item_id,
            provider_text,
            writer: None,
            external: None,
            sealed: None,
        }
    }

    fn from_shared(item_id: ItemId, text: &ReplyText, provider_text: bool) -> Self {
        Self {
            item_id,
            provider_text,
            writer: None,
            external: Some(text.clone()),
            sealed: None,
        }
    }

    fn append_shared(&mut self, text: &ReplyText) -> ReplyText {
        assert!(
            self.sealed.is_none(),
            "sealed reply accumulator cannot accept another delta"
        );
        if let Some(writer) = self.writer.as_mut() {
            return writer.append_shared(text);
        }
        if let Some(previous) = self.external.take() {
            if let Some(joined) = previous.try_join(text) {
                self.external = Some(joined);
                return text.clone();
            }
            let mut writer = if self.provider_text {
                ReplyArenaWriter::new().with_standard_provider_json_views()
            } else {
                ReplyArenaWriter::new()
            };
            let _ = writer.append_shared(&previous);
            let appended = writer.append_shared(text);
            self.writer = Some(writer);
            return appended;
        }
        self.external = Some(text.clone());
        text.clone()
    }

    fn snapshot(&self) -> ReplyText {
        self.sealed.clone().unwrap_or_else(|| {
            self.external.clone().unwrap_or_else(|| {
                self.writer
                    .as_ref()
                    .map_or_else(ReplyText::default, ReplyArenaWriter::snapshot)
            })
        })
    }

    fn seal(&mut self) -> ReplyText {
        if let Some(text) = &self.sealed {
            return text.clone();
        }
        let text = self.external.take().unwrap_or_else(|| {
            self.writer
                .take()
                .map_or_else(ReplyText::default, ReplyArenaWriter::seal)
        });
        self.sealed = Some(text.clone());
        text
    }

    fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }
}

/// Suppresses only the exact already-journaled prefix replayed by a resumed
/// provider stream. A divergence ends suppression immediately, preserving all
/// novel suffix content without relying on provider chunk boundaries.
#[derive(Debug, Default)]
struct ReplayPrefix {
    message: Option<ReplyText>,
    reasoning: Option<ReplyText>,
    refusal: String,
    message_applied: Option<ReplyText>,
    reasoning_applied: Option<ReplyText>,
    refusal_applied: String,
    structured_applied: Vec<StreamEvent>,
    structured_expected: VecDeque<StreamEvent>,
    response_epoch: u64,
}

impl ReplayPrefix {
    fn capture(
        &mut self,
        message: &Option<TextAccumulator>,
        reasoning: &Option<TextAccumulator>,
        refusal: &str,
    ) {
        if let Some(message) = message {
            self.message_applied = Some(message.snapshot());
        }
        if let Some(reasoning) = reasoning {
            self.reasoning_applied = Some(reasoning.snapshot());
        }
        if self.refusal_applied.is_empty() {
            self.refusal_applied = refusal.to_owned();
        }
        self.message.clone_from(&self.message_applied);
        self.reasoning.clone_from(&self.reasoning_applied);
        self.refusal.clone_from(&self.refusal_applied);
        self.structured_expected = normalized_structured_replay(&self.structured_applied);
    }

    fn filter_message(&mut self, text: ReplyText) -> ReplyText {
        strip_replayed_reply_prefix(&mut self.message, text)
    }

    fn filter_reasoning(&mut self, text: ReplyText) -> ReplyText {
        strip_replayed_reply_prefix(&mut self.reasoning, text)
    }

    fn filter_refusal(&mut self, text: String) -> String {
        let text = strip_replayed_prefix(&mut self.refusal, text);
        self.refusal_applied.push_str(&text);
        text
    }

    fn filter_structured(
        &mut self,
        event: StreamEvent,
    ) -> Result<Option<StreamEvent>, ProviderError> {
        if !is_structured_replay_event(&event) {
            if matches!(&event, StreamEvent::Finish { .. }) && !self.structured_expected.is_empty()
            {
                return Err(provider_protocol_error(
                    "provider structured replay ended before its durable prefix was restored",
                ));
            }
            return Ok(Some(event));
        }
        let Some(expected) = self.structured_expected.front() else {
            return Ok(Some(event));
        };
        if let (
            StreamEvent::ToolCallArgsDelta {
                call_id: expected_call,
                args_fragment: expected_args,
            },
            StreamEvent::ToolCallArgsDelta {
                call_id,
                args_fragment,
            },
        ) = (expected, &event)
        {
            if expected_call != call_id {
                return Err(provider_protocol_error(
                    "provider structured replay changed tool-call identity",
                ));
            }
            let (remaining, suffix) = strip_exact_replayed_prefix(expected_args, args_fragment)?;
            if remaining.is_empty() {
                self.structured_expected.pop_front();
            } else if let Some(StreamEvent::ToolCallArgsDelta { args_fragment, .. }) =
                self.structured_expected.front_mut()
            {
                *args_fragment = remaining;
            }
            return Ok(
                (!suffix.is_empty()).then(|| StreamEvent::ToolCallArgsDelta {
                    call_id: call_id.clone(),
                    args_fragment: suffix,
                }),
            );
        }
        if expected != &event {
            return Err(provider_protocol_error(
                "provider structured replay diverged from its durable prefix",
            ));
        }
        self.structured_expected.pop_front();
        Ok(None)
    }

    fn record_structured(&mut self, event: StreamEvent) {
        if let StreamEvent::ToolCallArgsDelta {
            call_id,
            args_fragment,
        } = &event
            && let Some(StreamEvent::ToolCallArgsDelta {
                call_id: previous_call,
                args_fragment: previous_args,
            }) = self.structured_applied.last_mut()
            && previous_call == call_id
        {
            previous_args.push_str(args_fragment);
            return;
        }
        self.structured_applied.push(event);
    }

    fn has_applied_content(&self) -> bool {
        self.message_applied
            .as_ref()
            .is_some_and(|text| !text.is_empty())
            || self
                .reasoning_applied
                .as_ref()
                .is_some_and(|text| !text.is_empty())
            || !self.refusal_applied.is_empty()
            || !self.structured_applied.is_empty()
    }

    /// A completed tool boundary is an external durability boundary even when
    /// its accumulator is empty by the time the response finishes. The replay
    /// ledger survives route retries/restart, so it is the authoritative place
    /// to retain this fact for the whole logical provider request.
    fn crossed_tool_boundary(&self) -> bool {
        self.structured_applied.iter().any(|event| {
            matches!(
                event,
                StreamEvent::ToolCallStart { .. }
                    | StreamEvent::ToolCallArgsDelta { .. }
                    | StreamEvent::ToolCallEnd { .. }
                    | StreamEvent::ServerToolUse { .. }
                    | StreamEvent::ServerToolResult { .. }
            )
        })
    }

    fn reset_for_next_request(&mut self) {
        self.message = None;
        self.reasoning = None;
        self.refusal.clear();
        self.message_applied = None;
        self.reasoning_applied = None;
        self.refusal_applied.clear();
        self.structured_applied.clear();
        self.structured_expected.clear();
        self.response_epoch = self.response_epoch.saturating_add(1);
    }
}

fn normalized_structured_replay(events: &[StreamEvent]) -> VecDeque<StreamEvent> {
    let mut normalized = Vec::<StreamEvent>::new();
    for event in events {
        if let StreamEvent::ToolCallArgsDelta {
            call_id,
            args_fragment,
        } = event
            && let Some(StreamEvent::ToolCallArgsDelta {
                call_id: previous_call,
                args_fragment: previous_args,
            }) = normalized.last_mut()
            && previous_call == call_id
        {
            previous_args.push_str(args_fragment);
        } else {
            normalized.push(event.clone());
        }
    }
    normalized.into()
}

fn strip_exact_replayed_prefix(
    expected: &str,
    incoming: &str,
) -> Result<(String, String), ProviderError> {
    let common = expected
        .bytes()
        .zip(incoming.bytes())
        .take_while(|(expected, incoming)| expected == incoming)
        .count();
    if common < expected.len().min(incoming.len()) {
        return Err(provider_protocol_error(
            "provider replay changed a durable tool-argument prefix",
        ));
    }
    if incoming.len() < expected.len() {
        Ok((expected[incoming.len()..].to_owned(), String::new()))
    } else {
        Ok((String::new(), incoming[expected.len()..].to_owned()))
    }
}

fn is_structured_replay_event(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::ProviderOpaque { .. }
            | StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallArgsDelta { .. }
            | StreamEvent::ToolCallEnd { .. }
            | StreamEvent::ServerToolUse { .. }
            | StreamEvent::ServerToolResult { .. }
            | StreamEvent::WebSources { .. }
    )
}

fn strip_replayed_prefix(expected: &mut String, text: String) -> String {
    if expected.is_empty() || text.is_empty() {
        return text;
    }
    let common_chars = expected
        .chars()
        .zip(text.chars())
        .take_while(|(expected, actual)| expected == actual)
        .count();
    let expected_bytes = expected
        .char_indices()
        .nth(common_chars)
        .map_or(expected.len(), |(index, _)| index);
    let text_bytes = text
        .char_indices()
        .nth(common_chars)
        .map_or(text.len(), |(index, _)| index);
    if text_bytes == text.len() && expected_bytes < expected.len() {
        expected.drain(..expected_bytes);
        String::new()
    } else {
        expected.clear();
        text[text_bytes..].to_owned()
    }
}

/// Removes the exact prefix already accepted into the canonical reply arena.
/// The expected side is only a shrinking range handle; the incoming provider
/// delta is returned in its original allocation after an in-place drain.
fn strip_replayed_reply_prefix(expected: &mut Option<ReplyText>, text: ReplyText) -> ReplyText {
    let Some(current) = expected.take() else {
        return text;
    };
    if current.is_empty() {
        return text;
    }
    if text.is_empty() {
        *expected = Some(current);
        return text;
    }

    let mut expected_bytes = current
        .segments()
        .into_iter()
        .flat_map(IntoIterator::into_iter);
    let mut incoming_bytes = text
        .segments()
        .into_iter()
        .flat_map(IntoIterator::into_iter);
    let mut common = 0_usize;
    loop {
        match (expected_bytes.next(), incoming_bytes.next()) {
            (Some(left), Some(right)) if left == right => common = common.saturating_add(1),
            _ => break,
        }
    }
    while common > 0 && text.slice(0..common).is_none() {
        common -= 1;
    }

    if common < current.len().min(text.len()) {
        expected.take();
        if let Some(suffix) = text.slice(common..text.len()) {
            return suffix;
        }
        debug_assert!(false, "common reply prefix must end at a UTF-8 boundary");
        return text;
    }
    if common < current.len() {
        *expected = current.slice(common..current.len());
        return ReplyText::default();
    }
    if let Some(suffix) = text.slice(common..text.len()) {
        return suffix;
    }
    debug_assert!(false, "complete replay prefix must end at a UTF-8 boundary");
    text
}

struct PendingUsageCommit {
    footprint: Option<ContextFootprint>,
    usage: Usage,
}

struct RequestAttemptMarkers {
    provider_view: Option<serde_json::Value>,
    cache: serde_json::Value,
    response_epoch: u64,
    request_budget: Option<RequestBudgetStatusV1>,
}

/// One as-yet-uncommitted provider-stream delta. It is intentionally limited
/// to the actor's text/reasoning/tool-argument stream; process output carries
/// raw bytes and is read directly by prompt reconstruction, so it stays
/// independently journaled at its daemon source.
#[derive(Debug)]
struct PendingItemDelta {
    run_id: RunId,
    item_id: ItemId,
    delta: ItemDelta,
}

/// One in-flight tool call; `args` collects the streamed JSON fragments.
#[derive(Debug)]
struct ToolAccumulator {
    item_id: ItemId,
    call_id: String,
    name: String,
    args: String,
    requested_name: Option<String>,
    parsed_args: OnceLock<Result<Arc<serde_json::Value>, String>>,
}

impl ToolAccumulator {
    fn correct_result(&self, mut result: BoundedResult) -> BoundedResult {
        if let Some(requested) = &self.requested_name {
            let mut preview = serde_json::from_str::<serde_json::Value>(&result.preview)
                .unwrap_or_else(|_| serde_json::Value::String(result.preview.clone()));
            if !preview.is_object() {
                preview = serde_json::json!({ "result": preview });
            }
            preview["tool_name_correction"] = serde_json::json!({
                "requested": requested,
                "resolved": self.name,
            });
            result.preview = preview.to_string();
        }
        result
    }
}

const TOOL_CALL_REPAIR_RESET_EXTENSION_KIND: &str = "tool_call_repair_reset";

fn repaired_tool_name(definitions: &[ToolDefinition], requested: &str) -> Option<String> {
    if definitions.iter().any(|tool| tool.name == requested) {
        return None;
    }
    let normalize = |name: &str| {
        name.bytes()
            .filter(|byte| *byte != b'_')
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>()
    };
    let requested = normalize(requested);
    let mut matches = definitions
        .iter()
        .filter(|tool| normalize(&tool.name) == requested);
    let matched = matches.next()?;
    matches.next().is_none().then(|| matched.name.clone())
}

pub(crate) fn invalid_tool_call_result(result: &BoundedResult) -> bool {
    matches!(
        result.data,
        Some(haider_protocol::tool::ToolResultData::InvalidToolCall { .. })
    )
}

struct RequestInputResolutionContext {
    request: RequestInput,
    menu: Menu,
    recovered_open_menu: bool,
}

enum GeneralToolOutcome {
    Completed(BoundedResult),
    Deferred(Box<DeferredTicket>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphFinalizationAnswer {
    ContinueWork,
    AbandonAndFinish,
    Reconsult,
}

#[derive(Debug, Clone)]
struct DeferredAccumulator {
    call_id: String,
    ticket: DeferredTicket,
    report_emitted: bool,
    child_result_emitted: bool,
    tool_result_emitted: bool,
    item_completed: bool,
}

fn parse_tool_args(tool: &ToolAccumulator) -> Result<Arc<serde_json::Value>, DriveError> {
    match tool.parsed_args.get_or_init(|| {
        if tool.args.is_empty() {
            Ok(Arc::new(serde_json::json!({})))
        } else {
            serde_json::from_str(&tool.args)
                .map_err(|error| error.to_string())
                .and_then(|args: serde_json::Value| {
                    if args.is_object() {
                        Ok(Arc::new(args))
                    } else {
                        Err("expected a JSON object, received a non-object JSON value".into())
                    }
                })
        }
    }) {
        Ok(args) => Ok(Arc::clone(args)),
        Err(error) => Err(DriveError::Provider(
            ProviderError::new(
                ProviderErrorKind::MalformedFrame,
                format!(
                    "tool call `{}` ended with malformed JSON arguments: {error}",
                    tool.call_id,
                ),
            )
            .with_presentation(ErrorPresentation::new(
                "malformed-tool-arguments",
                "Tool arguments were malformed",
                "The model did not produce a valid JSON object for this tool call.",
                ErrorScope::Tool,
                [ErrorAction::Retry],
            )),
        )),
    }
}

fn validate_permission_selection(menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
    let option = if let Some(key) = answer.option_key.as_deref() {
        menu.options.iter().find(|option| option.key == key)
    } else {
        usize::try_from(answer.option_index)
            .ok()
            .and_then(|index| menu.options.get(index))
    }
    .ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "permission answer does not select a server-enumerated option",
            false,
        )
    })?;
    if option.decision.is_none() {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("permission option `{}` has no decision", option.key),
            false,
        ));
    }
    Ok(())
}

fn provider_tool_block(tools: &[ToolAccumulator], call_id: &str) -> Result<Block, DriveError> {
    let Some(tool) = tools.iter().find(|tool| tool.call_id == call_id) else {
        return Err(DriveError::Provider(provider_protocol_error(format!(
            "provider ended unknown tool call `{call_id}`",
        ))));
    };
    Ok(Block::ToolCall {
        call_id: tool.call_id.clone(),
        name: tool.name.clone(),
        args: parse_tool_args(tool)?.as_ref().clone(),
    })
}

fn tool_args_or_raw(tool: &ToolAccumulator) -> serde_json::Value {
    if let Some(parsed) = tool.parsed_args.get() {
        return match parsed {
            Ok(args) => args.as_ref().clone(),
            Err(_) => serde_json::Value::String(tool.args.clone()),
        };
    }
    if tool.args.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(&tool.args)
        .unwrap_or_else(|_| serde_json::Value::String(tool.args.clone()))
}

fn tool_call_within_advertised_ceiling(config: &HarnessConfig, name: &str) -> bool {
    if config.agent_id.is_none() && !config.enforce_advertised_tool_ceiling {
        return true;
    }
    config
        .tool_definitions()
        .iter()
        .any(|definition| definition.name == name)
}

fn provider_protocol_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedFrame, message)
}

fn provider_view_invariant_error(
    error: haider_provider::ProviderViewInvariantError,
) -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        format!("provider request blocked before send: {error}"),
        false,
    )
    .with_presentation(ErrorPresentation::new(
        "provider-prefix-invariant",
        "Provider cache prefix changed unexpectedly",
        "Haider blocked this request because previously sent provider bytes changed inside a live cache epoch. Start a deliberate new cache epoch or repair the conversation store before retrying.",
        ErrorScope::Turn,
        [ErrorAction::RetryFresh],
    ))
}

fn provider_stream_interrupted(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::StreamInterrupted, message)
}

const REFUSAL_REASON_CAP: usize = 512;

fn append_bounded_refusal(reason: &mut String, delta: &str) {
    if reason.len() >= REFUSAL_REASON_CAP {
        return;
    }
    let normalized: String = delta
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let remaining = REFUSAL_REASON_CAP.saturating_sub(reason.len());
    let boundary = normalized
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= remaining)
        .last()
        .unwrap_or(0);
    if normalized.len() <= remaining {
        reason.push_str(&normalized);
    } else {
        reason.push_str(&normalized[..boundary]);
    }
}

fn normalized_refusal_reason(reason: &str) -> String {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        "The model declined to answer this request.".into()
    } else {
        reason
    }
}

fn repeated_context_overflow_after_compaction() -> DriveError {
    DriveError::Provider(ProviderError::new(
        ProviderErrorKind::ContextExceeded,
        "provider context overflow repeated after compaction",
    ))
}

fn compaction_runaway_guard_error(before: u64, after: u64, input_budget: u64) -> DriveError {
    let freed = before.saturating_sub(after);
    DriveError::Provider(ProviderError::new(
        ProviderErrorKind::ContextExceeded,
        format!(
            "context compaction guard stopped an ineffective retry: used {before} -> {after} \
             tokens (freed {freed}); input budget is {input_budget} and the minimum reduction is \
             {COMPACTION_MIN_FREED_PERCENT}%"
        ),
    ))
}

fn compaction_guard_repeat_error(used: u64, input_budget: u64) -> DriveError {
    DriveError::Provider(ProviderError::new(
        ProviderErrorKind::ContextExceeded,
        format!(
            "context compaction guard refused a second compaction in this turn: provider input \
             estimate {used} exceeds the promoted budget {input_budget}"
        ),
    ))
}

fn request_budget_error(status: &RequestBudgetStatusV1) -> HaiderError {
    let continuation = if let Some(agent) = &status.continuation.agent_id {
        format!("continue with message_subagent for agent {agent}")
    } else if let Some(branch) = &status.continuation.branch_id {
        format!(
            "continue with a new turn on branch {branch} in session {}",
            status.continuation.session_id
        )
    } else {
        format!(
            "continue in this session or run `haider run --resume {}`",
            status.continuation.run_id
        )
    };
    let mut error = HaiderError::new(
        ErrorCode::RequestBudgetExceeded,
        format!("{}; {continuation}", status.summary()),
        false,
    );
    error.details = Some(serde_json::json!(status));
    error
}

fn continuation_limit_error(count: usize, limit: usize) -> HaiderError {
    let mut error = HaiderError::new(
        ErrorCode::LoopLimit,
        format!("provider continuation limit exceeded at continuation {count} (limit {limit})"),
        false,
    );
    error.details = Some(serde_json::json!({
        "continuation_count": count,
        "continuation_limit": limit,
    }));
    error
}

fn submit_busy_error(capacity: usize) -> HaiderError {
    let mut error = HaiderError::new(
        ErrorCode::Busy,
        format!("session deferred submission queue is full (capacity {capacity})"),
        true,
    );
    error.details = Some(serde_json::json!({
        "deferred_command_capacity": capacity,
    }));
    error
}

fn delegated_child_wait_timed_out(error: &HaiderError) -> bool {
    error.code == ErrorCode::ProviderTimeout
        && error
            .details
            .as_ref()
            .and_then(|details| details.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("delegated_child_wait_timeout")
}

fn provider_error_to_haider(provider_error: ProviderError) -> HaiderError {
    let code = if provider_error.presentation.subcode.as_str() == "provider-timeout" {
        ErrorCode::ProviderTimeout
    } else {
        ErrorCode::ProviderError
    };
    let mut error = HaiderError::new(code, provider_error.to_string(), provider_error.retryable);
    error.details = Some(serde_json::json!({
        "provider_error_kind": format!("{:?}", provider_error.kind),
        "retry_after_ms": provider_error.retry_after_ms,
        "opened_within_ms": provider_error.opened_within_ms,
        "budget_ms": provider_error.budget_ms,
        "reason": provider_error.timeout_reason,
    }));
    error.presentation = Some(provider_error.presentation);
    error
}

/// Converts exactly one absolute-deadline outcome into its durable provider
/// terminal class. Returning `true` means retry state owned the expiry and the
/// daemon's in-flight deadline mapper must not replace the provider failure.
fn classify_provider_deadline_terminal(
    error: &mut ProviderError,
    run_state: Option<&RunState>,
    retry_admission_in_progress: bool,
) -> bool {
    if error.timeout_reason != Some(ProviderTimeoutReason::DeadlineExhausted)
        || (!retry_admission_in_progress && !run_state_is_provider_retry(run_state))
    {
        return false;
    }

    error.message =
        "provider retry budget expired before another provider request was in flight".into();
    error.retryable = false;
    let mut presentation = ErrorPresentation::new(
        "provider-retry-exhausted",
        "Provider retry budget exhausted",
        "The run deadline expired during provider retry backoff; another request cannot be admitted.",
        ErrorScope::Turn,
        [ErrorAction::None],
    );
    copy_provider_metadata(&mut presentation, &error.presentation);
    error.presentation = presentation;
    true
}

fn run_state_is_provider_retry(run_state: Option<&RunState>) -> bool {
    matches!(
        run_state,
        Some(
            RunState::Retrying { .. }
                | RunState::Waiting {
                    reason: WaitReason::RateLimit | WaitReason::ProviderBackoff,
                }
        )
    )
}

fn specialize_provider_presentation(auth_scope: &str, error: &mut ProviderError) {
    if error.kind != ProviderErrorKind::Authentication {
        return;
    }
    if matches!(
        error.presentation.subcode.as_str(),
        "account-revoked" | "account-deleted"
    ) {
        return;
    }
    let provider_detail = error.presentation.detail.clone();
    let mut specialized = match auth_scope {
        "api_key" => ErrorPresentation::new(
            "invalid-api-key",
            "API key rejected",
            "The provider rejected the active API key.",
            ErrorScope::Account,
            [ErrorAction::EditKey, ErrorAction::SwitchAccount],
        ),
        "oauth_subscription" | "cloud_bearer" => ErrorPresentation::new(
            "oauth-expired",
            "Sign-in expired",
            "The active OAuth credential could not be refreshed.",
            ErrorScope::Account,
            [
                ErrorAction::Relogin,
                ErrorAction::Reimport,
                ErrorAction::SwitchAccount,
            ],
        ),
        _ => return,
    };
    specialized.detail = provider_detail;
    copy_provider_metadata(&mut specialized, &error.presentation);
    error.presentation = specialized;
}

fn stream_interruption_presentation(error: &ProviderError) -> ErrorPresentation {
    let mut presentation = ErrorPresentation::new(
        "stream-interrupted",
        "Response stream interrupted",
        format!(
            "{} The provider connection ended after part of the response was received.",
            error.presentation.detail,
        ),
        ErrorScope::Turn,
        [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
    );
    copy_provider_metadata(&mut presentation, &error.presentation);
    presentation
}

fn selected_error_action(menu: &Menu, answer: &MenuAnswer) -> Result<ErrorAction, HaiderError> {
    let MenuKind::ErrorRecovery { option_actions, .. } = &menu.kind else {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "recovery answer targeted a non-recovery menu",
            false,
        ));
    };
    let selected = if let Some(key) = answer.option_key.as_deref() {
        menu.options
            .iter()
            .enumerate()
            .find(|(_, option)| option.key == key)
    } else {
        usize::try_from(answer.option_index)
            .ok()
            .and_then(|index| menu.options.get(index).map(|option| (index, option)))
    }
    .ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "recovery answer does not select a server-enumerated option",
            false,
        )
    })?;
    let action = option_actions.get(selected.0).copied().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "recovery menu action metadata does not match its options",
            false,
        )
    })?;
    if selected.1.key != error_action_key(action) {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "recovery option key does not match its typed action",
            false,
        ));
    }
    Ok(action)
}

fn automatic_error_action_answer(
    menu: &Menu,
    requested: ErrorAction,
) -> Result<MenuAnswer, HaiderError> {
    let MenuKind::ErrorRecovery { option_actions, .. } = &menu.kind else {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "automatic recovery targeted a non-recovery menu",
            false,
        ));
    };
    let (index, option) = option_actions
        .iter()
        .enumerate()
        .find(|(_, action)| **action == requested)
        .and_then(|(index, _)| menu.options.get(index).map(|option| (index, option)))
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "automatic recovery action is not enumerated by the menu",
                false,
            )
        })?;
    if option.key != error_action_key(requested) {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "automatic recovery option key does not match its typed action",
            false,
        ));
    }
    Ok(MenuAnswer {
        menu: menu.id.clone(),
        option_key: Some(option.key.clone()),
        option_index: u32::try_from(index).map_err(|_| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                "automatic recovery option index exceeds protocol bounds",
                false,
            )
        })?,
        value: None,
        via: haider_protocol::menu::AnswerVia::Hook,
    })
}

fn copy_provider_metadata(target: &mut ErrorPresentation, source: &ErrorPresentation) {
    target.provider_http_status = source.provider_http_status;
    target
        .provider_request_id
        .clone_from(&source.provider_request_id);
    target.retry_after_ms = source.retry_after_ms;
    target.reset_at_ms = source.reset_at_ms;
    target.opened_within_ms = source.opened_within_ms;
    target.budget_ms = source.budget_ms;
}

fn recovery_card_kind(presentation: &ErrorPresentation) -> Option<ErrorRecoveryCardKind> {
    match presentation.subcode.as_str() {
        "oauth-expired" | "reimport-required" => Some(ErrorRecoveryCardKind::OauthExpired),
        "invalid-api-key" => Some(ErrorRecoveryCardKind::InvalidApiKey),
        "account-revoked" => Some(ErrorRecoveryCardKind::AccountRevoked),
        "account-deleted" | "account-unavailable" => Some(ErrorRecoveryCardKind::AccountDeleted),
        "rate-limited" => Some(ErrorRecoveryCardKind::RateLimit),
        "quota-exhausted" => Some(ErrorRecoveryCardKind::QuotaExhausted),
        "keychain-relink-required" => Some(ErrorRecoveryCardKind::KeychainRelink),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn recovery_menu(
    id: MenuId,
    source_run: &RunId,
    source_item: Option<ItemId>,
    card: ErrorRecoveryCardKind,
    presentation: ErrorPresentation,
    provider: Option<String>,
    account: Option<CredentialAlias>,
    blocking: bool,
) -> Menu {
    let mut body = vec![presentation.detail.clone()];
    let title = presentation.title.clone();
    if let Some(status) = presentation.provider_http_status {
        body.push(format!("Provider HTTP status: {status}"));
    }
    if let Some(request_id) = &presentation.provider_request_id {
        body.push(format!("Request ID: {request_id}"));
    }
    if let Some(retry_after_ms) = presentation.retry_after_ms {
        let seconds = retry_after_ms.div_ceil(1_000);
        body.push(presentation.reset_at_ms.map_or_else(
            || format!("Retry countdown: {seconds}s."),
            |reset| format!("Retry countdown: {seconds}s (reset at Unix time {reset} ms)."),
        ));
    } else if let Some(reset_at_ms) = presentation.reset_at_ms {
        body.push(format!("Available again after Unix time {reset_at_ms} ms."));
    }
    let option_actions = presentation
        .allowed_actions
        .iter()
        .copied()
        .filter(|action| *action != ErrorAction::None)
        .collect::<Vec<_>>();
    let options = option_actions
        .iter()
        .map(|action| MenuOption {
            key: error_action_key(*action).to_owned(),
            label: error_action_label(*action).to_owned(),
            detail: error_action_detail(*action).map(str::to_owned),
            decision: None,
        })
        .collect();
    Menu {
        id,
        kind: MenuKind::ErrorRecovery {
            card,
            presentation,
            option_actions,
            provider,
            account,
            source_run: Some(source_run.clone()),
            source_item,
        },
        title,
        body,
        options,
        blocking,
        scope: MenuScope::Session,
        origin: "error-recovery".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn error_action_key(action: ErrorAction) -> &'static str {
    match action {
        ErrorAction::Retry => "retry",
        ErrorAction::Relogin => "relogin",
        ErrorAction::Reimport => "reimport",
        ErrorAction::EditKey => "edit_key",
        ErrorAction::SwitchAccount => "switch_account",
        ErrorAction::TopUp => "top_up",
        ErrorAction::Wait => "wait",
        ErrorAction::ChooseModel => "choose_model",
        ErrorAction::ContactAdmin => "contact_admin",
        ErrorAction::ContinuePartial => "continue_partial",
        ErrorAction::RetryFresh => "retry_fresh",
        ErrorAction::None => "none",
    }
}

fn error_action_label(action: ErrorAction) -> &'static str {
    match action {
        ErrorAction::Retry => "Retry",
        ErrorAction::Relogin => "Re-login",
        ErrorAction::Reimport => "Re-import",
        ErrorAction::EditKey => "Edit key",
        ErrorAction::SwitchAccount => "Switch account",
        ErrorAction::TopUp => "Top up",
        ErrorAction::Wait => "Wait",
        ErrorAction::ChooseModel => "Choose model",
        ErrorAction::ContactAdmin => "Contact admin",
        ErrorAction::ContinuePartial => "Continue from partial",
        ErrorAction::RetryFresh => "Retry from scratch",
        ErrorAction::None => "Dismiss",
    }
}

fn error_action_detail(action: ErrorAction) -> Option<&'static str> {
    match action {
        ErrorAction::TopUp => Some("Add credits in the provider billing portal, then retry."),
        ErrorAction::SwitchAccount => Some("Open accounts and choose another usable account."),
        ErrorAction::Relogin => Some("Start the provider sign-in flow again."),
        ErrorAction::Reimport => Some("Re-adopt the provider credential from its local source."),
        ErrorAction::EditKey => Some("Enter and validate a replacement API key."),
        ErrorAction::Wait => Some("Wait until the displayed reset time before retrying."),
        ErrorAction::ContinuePartial => Some("Continue without repeating the partial response."),
        ErrorAction::RetryFresh => Some("Start over; keep the partial response only as history."),
        ErrorAction::Retry
        | ErrorAction::ChooseModel
        | ErrorAction::ContactAdmin
        | ErrorAction::None => None,
    }
}

/// Safe fallback for non-provider failures and pre-E2 `HaiderError`
/// producers. New provider errors carry their richer presentation directly.
#[must_use]
pub fn presentation_for_haider_error(error: &HaiderError) -> ErrorPresentation {
    if let Some(presentation) = &error.presentation {
        return presentation.clone();
    }
    ErrorPresentation::new(
        error.code.as_subcode(),
        "Haider could not complete the turn",
        sanitized_failure_message(&error.message),
        ErrorScope::Turn,
        [if error.retryable {
            ErrorAction::Retry
        } else {
            ErrorAction::None
        }],
    )
}

fn provider_error_allows_retry(
    error: &mut ProviderError,
    deadline: Option<tokio::time::Instant>,
    run_id: &RunId,
    failed_attempt: usize,
) -> bool {
    if !error.retryable
        || !matches!(
            error.kind,
            ProviderErrorKind::NetworkUnavailable
                | ProviderErrorKind::Transport
                | ProviderErrorKind::StreamInterrupted
                | ProviderErrorKind::RateLimited
                | ProviderErrorKind::Overloaded
        )
    {
        return false;
    }
    // A locally bounded response-open wait is still retryable by the caller,
    // but an automatic replay would start the same parked request again.
    // Preserve the typed Retry action while terminalizing this attempt under
    // Haider's own control, independent of the configured timeout value.
    if error.timeout_reason == Some(ProviderTimeoutReason::ResponseOpen) {
        return false;
    }
    let Some(deadline) = deadline else {
        return true;
    };
    let delay_ms = error
        .retry_after_ms
        .unwrap_or_else(|| retry_jittered_backoff_ms(run_id, failed_attempt));
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let delay = std::time::Duration::from_millis(delay_ms);
    // The retry sleeper is outside `before_provider_request_deadline`, so it
    // must enforce the enclosing deadline itself. The ordinary provider
    // margin protects the next request-open cutoff; retry admission reserves
    // one additional equal interval for scheduler handoff and durable terminal
    // delivery. Sharing only the ordinary margin let a barely admitted retry
    // reach the next open at its cutoff, racing this provider failure against
    // the caller's wall timeout. Equality is already hopeless and is refused.
    // This is exhaustion for the accepted run: latch a non-retryable terminal
    // and suppress an action that the same absolute deadline cannot honor. A
    // newly accepted turn receives a fresh provider error/budget.
    if !provider_retry_fits_deadline(remaining, delay) {
        terminalize_provider_retry(error);
        return false;
    }
    let remaining_after_delay = remaining.saturating_sub(delay);
    if error.presentation.subcode.as_str() != "provider-timeout" {
        return true;
    }
    let Some(budget_ms) = error.budget_ms else {
        terminalize_provider_retry(error);
        return false;
    };
    let provider_budget = std::time::Duration::from_millis(budget_ms);
    let can_retry = effective_request_budget(
        provider_budget,
        Some(remaining_after_delay),
        PROVIDER_DEADLINE_SAFETY_MARGIN,
    )
    .is_ok_and(|selected| selected == provider_budget);
    if !can_retry {
        terminalize_provider_retry(error);
    }
    can_retry
}

fn provider_retry_fits_deadline(
    remaining: std::time::Duration,
    delay: std::time::Duration,
) -> bool {
    let admission_margin =
        PROVIDER_DEADLINE_SAFETY_MARGIN.saturating_add(PROVIDER_DEADLINE_SAFETY_MARGIN);
    remaining.saturating_sub(delay) > admission_margin
}

fn terminalize_provider_retry(error: &mut ProviderError) {
    error.retryable = false;
    error.presentation.allowed_actions = vec![ErrorAction::None];
}

/// Transport failures never require a user decision. Before any provider
/// content they may follow the bounded automatic retry path; after content has
/// streamed they must terminate as a typed run failure because replay could
/// duplicate visible output or side effects.
fn provider_error_is_transport_fault(error: &ProviderError) -> bool {
    matches!(
        error.kind,
        ProviderErrorKind::NetworkUnavailable
            | ProviderErrorKind::Transport
            | ProviderErrorKind::StreamInterrupted
    )
}

fn provider_error_waits_for_route(
    error: &ProviderError,
    route_status: haider_platform::RouteStatus,
) -> bool {
    matches!(
        error.kind,
        ProviderErrorKind::NetworkUnavailable | ProviderErrorKind::StreamInterrupted
    ) && route_status == haider_platform::RouteStatus::Unavailable
        && error.timeout_reason.is_none()
        && error.presentation.provider_http_status.is_none()
}

fn provider_error_allows_rotation(error: &ProviderError) -> bool {
    match error.kind {
        ProviderErrorKind::RateLimited => error
            .retry_after_ms
            .is_some_and(|delay| delay <= MAX_PROVIDER_RETRY_AFTER_MS),
        ProviderErrorKind::Authentication => true,
        ProviderErrorKind::PermissionDenied
        | ProviderErrorKind::Overloaded
        | ProviderErrorKind::ContextExceeded
        | ProviderErrorKind::InvalidRequest
        | ProviderErrorKind::NetworkUnavailable
        | ProviderErrorKind::Transport
        | ProviderErrorKind::MalformedFrame
        | ProviderErrorKind::InvalidUtf8
        | ProviderErrorKind::Internal
        | ProviderErrorKind::QuotaExhausted
        | ProviderErrorKind::StreamInterrupted
        | ProviderErrorKind::ConnectionConfiguration => false,
    }
}

/// Cross-provider fallback is a provider-health recovery only. Request,
/// context, and permission failures belong to the selected pair and must
/// never be laundered into a silent provider change.
fn provider_error_allows_pair_fallback(error: &ProviderError) -> bool {
    matches!(
        error.kind,
        ProviderErrorKind::Authentication
            | ProviderErrorKind::RateLimited
            | ProviderErrorKind::Overloaded
            | ProviderErrorKind::QuotaExhausted
    )
}

fn accepted_opaque_provider(provider_name: &str) -> &'static str {
    match provider_name {
        haider_provider::ANTHROPIC_PROVIDER_NAME
        | haider_provider::ANTHROPIC_OAUTH_PROVIDER_NAME => {
            haider_provider::ANTHROPIC_PROVIDER_NAME
        }
        haider_provider::OPENAI_PROVIDER_NAME | haider_provider::OPENAI_OAUTH_PROVIDER_NAME => {
            haider_provider::OPENAI_PROVIDER_NAME
        }
        haider_provider::GEMINI_PROVIDER_NAME => haider_provider::GEMINI_PROVIDER_NAME,
        _ => haider_provider::OPENAI_COMPATIBLE_PROVIDER_NAME,
    }
}

/// Drops opaque continuation blocks minted by a different wire family and
/// remaps every live compiler boundary across messages that become empty.
fn remap_after_provider_opaque_strip(
    messages: &mut Vec<Message>,
    provider_name: &str,
    stable_history_end: &mut usize,
    current_turn_start: &mut usize,
    latest_compaction_summary_end: &mut Option<usize>,
) {
    let accepted = accepted_opaque_provider(provider_name);
    let mut boundary_map = Vec::with_capacity(messages.len().saturating_add(1));
    boundary_map.push(0usize);
    let mut retained = 0usize;
    for message in messages.iter_mut() {
        message.blocks.retain(|block| {
            !matches!(
                block,
                Block::ProviderOpaque { provider, .. } if provider != accepted
            )
        });
        retained = retained.saturating_add(usize::from(!message.blocks.is_empty()));
        boundary_map.push(retained);
    }
    messages.retain(|message| !message.blocks.is_empty());
    let remap = |boundary: usize| {
        boundary_map
            .get(boundary.min(boundary_map.len().saturating_sub(1)))
            .copied()
            .unwrap_or(messages.len())
    };
    *stable_history_end = remap(*stable_history_end);
    *current_turn_start = remap(*current_turn_start);
    *latest_compaction_summary_end = latest_compaction_summary_end.map(remap);
}

fn digest_json(value: &impl serde::Serialize) -> String {
    serde_json::to_vec(value).map_or_else(
        |_| blake3::hash(b"serialization-error").to_hex().to_string(),
        |bytes| blake3::hash(&bytes).to_hex().to_string(),
    )
}

fn usage_prefix_digests(config: &HarnessConfig, immutable_history: &[Message]) -> PrefixDigests {
    PrefixDigests {
        system: digest_json(&config.system_prompt),
        tools: config.canonical_tool_pack_digest(),
        immutable_history: digest_json(&immutable_history),
        model: digest_json(&config.model),
        auth_mode: digest_json(&config.usage_scope.auth_scope),
        reasoning_settings: digest_json(&config.reasoning_settings),
    }
}

fn keyed_breakpoint_hash(
    key: &CacheDiagnosticKey,
    breakpoint: &str,
    components: &[&str],
) -> String {
    let mut hasher = blake3::Hasher::new_keyed(&key.0);
    hasher.update(b"haider.cache-request-diagnostic.v1\0");
    hasher.update(&(breakpoint.len() as u64).to_le_bytes());
    hasher.update(breakpoint.as_bytes());
    for component in components {
        hasher.update(&(component.len() as u64).to_le_bytes());
        hasher.update(component.as_bytes());
    }
    format!("blake3-keyed:{}", hasher.finalize().to_hex())
}

fn cache_breakpoint_hashes(
    key: &CacheDiagnosticKey,
    digests: &PrefixDigests,
) -> CacheBreakpointHashesV1 {
    CacheBreakpointHashesV1 {
        system: keyed_breakpoint_hash(key, "system", &[&digests.system]),
        tools: keyed_breakpoint_hash(key, "tools", &[&digests.system, &digests.tools]),
        history: keyed_breakpoint_hash(
            key,
            "history",
            &[&digests.system, &digests.tools, &digests.immutable_history],
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_cache_request_diagnostic(
    key: &CacheDiagnosticKey,
    provider: &str,
    model: &str,
    cache_epoch: &str,
    current: &PrefixDigests,
    current_through_previous_history: Option<&PrefixDigests>,
    previous: Option<&PreviousCacheRequest>,
    history_message_count: usize,
    stable_prefix_tokens: u64,
    reuse_gap_ms: Option<u64>,
    control: CacheControlObservationV1,
    rewarm: Option<CacheRewarmReasonV1>,
) -> CacheRequestDiagnosticV1 {
    let breakpoint_hashes = cache_breakpoint_hashes(key, current);
    let cache_domain_hash = keyed_breakpoint_hash(key, "cache-domain", &[cache_epoch]);
    let cache_domain_changed = previous
        .and_then(|previous| previous.cache_domain_hash.as_ref())
        .map(|previous| previous != &cache_domain_hash);
    let previous_breakpoint = previous.map(|previous| PreviousCacheBreakpointV1 {
        message_count: u64::try_from(previous.history_message_count).unwrap_or(u64::MAX),
        expected_hash: previous.breakpoint_hashes.history.clone(),
        actual_hash: current_through_previous_history
            .map(|digests| cache_breakpoint_hashes(key, digests).history),
    });
    let prefix_match = previous.map_or(CachePrefixMatchV1::Unavailable, |previous| {
        if previous.breakpoint_hashes.system != breakpoint_hashes.system {
            CachePrefixMatchV1::Changed {
                first: CacheBreakpointV1::System,
            }
        } else if previous.breakpoint_hashes.tools != breakpoint_hashes.tools {
            CachePrefixMatchV1::Changed {
                first: CacheBreakpointV1::Tools,
            }
        } else {
            match previous_breakpoint
                .as_ref()
                .and_then(|breakpoint| breakpoint.actual_hash.as_ref())
            {
                Some(actual) if actual == &previous.breakpoint_hashes.history => {
                    CachePrefixMatchV1::Same
                }
                Some(_) => CachePrefixMatchV1::Changed {
                    first: CacheBreakpointV1::History,
                },
                None => CachePrefixMatchV1::Unavailable,
            }
        }
    });
    CacheRequestDiagnosticV1 {
        history_message_count: u64::try_from(history_message_count).unwrap_or(u64::MAX),
        stable_prefix_tokens,
        breakpoint_hashes,
        cache_domain_hash: Some(cache_domain_hash),
        cache_domain_changed,
        previous_breakpoint,
        prefix_match,
        control,
        cacheable_minimum_tokens: haider_provider::cacheable_prompt_minimum(provider, model),
        reuse_gap_ms,
        rewarm,
        classification: None,
    }
}

pub fn classify_cache_request(
    diagnostic: &CacheRequestDiagnosticV1,
    normalized: Option<&NormalizedUsage>,
) -> Option<CacheMissClassificationV1> {
    let Some(usage) = normalized else {
        return Some(CacheMissClassificationV1::Unavailable);
    };
    if usage.cache_status != CacheStatAvailability::Present {
        return Some(CacheMissClassificationV1::Unavailable);
    }
    if usage.cache_read_input > 0 {
        return None;
    }
    if let Some(rewarm) = diagnostic.rewarm {
        return Some(match rewarm {
            CacheRewarmReasonV1::PlannedCompaction => CacheMissClassificationV1::PlannedCompaction,
            CacheRewarmReasonV1::ConfigurationChange => {
                CacheMissClassificationV1::ConfigurationChange
            }
            _ => CacheMissClassificationV1::Unavailable,
        });
    }
    if let CachePrefixMatchV1::Changed { first } = diagnostic.prefix_match {
        return Some(CacheMissClassificationV1::PrefixChanged { first });
    }
    if diagnostic.cache_domain_changed == Some(true) {
        return Some(CacheMissClassificationV1::ConfigurationChange);
    }
    if diagnostic
        .cacheable_minimum_tokens
        .is_some_and(|minimum| diagnostic.stable_prefix_tokens < minimum)
    {
        return Some(CacheMissClassificationV1::BelowMinimum);
    }
    if let CacheControlObservationV1::NotEmitted { reason } = diagnostic.control {
        return Some(CacheMissClassificationV1::ControlNotEmitted { reason });
    }
    if let CacheControlObservationV1::Emitted { ttl_ms: Some(ttl) } = diagnostic.control
        && let Some(gap) = diagnostic.reuse_gap_ms
    {
        if gap > ttl {
            return Some(CacheMissClassificationV1::Expired);
        }
        if diagnostic.prefix_match == CachePrefixMatchV1::Same
            && diagnostic.cache_domain_changed == Some(false)
        {
            return Some(CacheMissClassificationV1::SamePrefixInTtl);
        }
    }
    Some(CacheMissClassificationV1::Unavailable)
}

#[derive(Clone, Copy)]
struct PromptCacheBoundaries {
    stable_history_end: usize,
    cacheable_history_end: usize,
    current_user_start: usize,
    previous_stable_history_end: Option<usize>,
    latest_compaction_summary_end: Option<usize>,
}

fn prompt_cache_metadata(
    config: &HarnessConfig,
    messages: &[Message],
    boundaries: PromptCacheBoundaries,
    prefix_digests: PrefixDigests,
    account_scope: Option<&CredentialAlias>,
    volatile_context_epoch: Option<&str>,
) -> PromptCacheMetadata {
    let PromptCacheBoundaries {
        stable_history_end,
        cacheable_history_end,
        current_user_start,
        previous_stable_history_end,
        latest_compaction_summary_end,
    } = boundaries;
    let stable_history_end = stable_history_end.min(messages.len());
    let cacheable_history_end = cacheable_history_end
        .max(stable_history_end)
        .min(messages.len());
    let current_user_start = current_user_start.min(messages.len());
    let latest_compaction_summary_end = latest_compaction_summary_end
        .filter(|boundary| *boundary > 0 && *boundary <= stable_history_end);
    let compaction_epoch = latest_compaction_summary_end.map_or_else(
        || digest_json(&"root-compaction-epoch"),
        |boundary| digest_json(&messages[boundary - 1]),
    );
    let cache_epoch = digest_json(&serde_json::json!({
        "provider": config.usage_scope.provider,
        "model": config.model,
        "max_tokens": config.max_tokens,
        "account_scope": account_scope,
        "system_digest": prefix_digests.system,
        "tool_digest": prefix_digests.tools,
        "auth_digest": prefix_digests.auth_mode,
        "reasoning_digest": prefix_digests.reasoning_settings,
        "compaction_epoch": compaction_epoch,
        "volatile_context_epoch": volatile_context_epoch,
    }));
    let stable_prefix_tokens =
        estimated_request_input_tokens(config, &messages[..cacheable_history_end]);
    PromptCacheMetadata {
        stable_history_end,
        cacheable_history_end: (cacheable_history_end != stable_history_end)
            .then_some(cacheable_history_end),
        current_user_start,
        previous_stable_history_end,
        latest_compaction_summary_end,
        prefix_digests,
        cache_epoch,
        header_epoch: String::new(),
        compaction_epoch,
        provider: config.usage_scope.provider.clone(),
        session_scope: config.session_id.as_str().to_owned(),
        cache_cohort: config.cache_cohort.clone(),
        account_scope: account_scope.map(|scope| scope.as_str().to_owned()),
        stable_prefix_tokens,
        expected_later_reads: config.cache_expected_later_reads,
        // The daemon measured this gap for the initially resolved account.
        // A pre-first-event account rotation creates a different cache
        // domain, so retain the conservative unknown-gap/5m fallback until a
        // later turn can measure that account from durable usage telemetry.
        reuse_gap_ms: (account_scope == config.usage_account.as_ref())
            .then_some(config.cache_reuse_gap_ms)
            .flatten(),
    }
}

fn attach_usage_scope_and_cost(
    config: &HarnessConfig,
    run_id: &RunId,
    cache_epoch: &str,
    stable_prefix_tokens: u64,
    usage: &mut Usage,
) {
    let mut scope = config.usage_scope.clone();
    scope.cache_epoch = cache_epoch.to_owned();
    scope.stable_prefix_tokens = stable_prefix_tokens;
    scope.run = Some(run_id.clone());
    scope.agent = config.agent_id.clone();
    if scope.agent.is_some() && scope.request_kind == UsageRequestKind::MainTurn {
        scope.request_kind = UsageRequestKind::DelegatedAgent;
    }
    // Legacy scopes may contain plain component hashes. New writers retain
    // only the keyed breakpoint/domain fingerprints in `Usage.request` so
    // short prompt components cannot be guessed offline from the journal.
    scope.prefix_digests = None;
    usage.cache_cost = usage.normalized.as_ref().and_then(|normalized| {
        haider_provider::estimate_cache_input_costs_for(
            &config.usage_scope.provider,
            &config.model,
            normalized,
        )
    });
    usage.scope = Some(scope);
}

fn cumulative_normalized(
    completed: &NormalizedUsage,
    current: &NormalizedUsage,
) -> NormalizedUsage {
    let combine_status = |left, right| {
        if left == CacheStatAvailability::Present && right == CacheStatAvailability::Present {
            CacheStatAvailability::Present
        } else {
            CacheStatAvailability::Unavailable
        }
    };
    let reasoning_accounting = if completed.reasoning_accounting == current.reasoning_accounting {
        current.reasoning_accounting
    } else {
        haider_protocol::provider::ReasoningAccounting::Unavailable
    };
    NormalizedUsage {
        logical_input: completed
            .logical_input
            .saturating_add(current.logical_input),
        uncached_input: completed
            .uncached_input
            .saturating_add(current.uncached_input),
        cache_read_input: completed
            .cache_read_input
            .saturating_add(current.cache_read_input),
        cache_write_input: completed
            .cache_write_input
            .saturating_add(current.cache_write_input),
        cache_write_5m_input: completed
            .cache_write_5m_input
            .saturating_add(current.cache_write_5m_input),
        cache_write_1h_input: completed
            .cache_write_1h_input
            .saturating_add(current.cache_write_1h_input),
        billed_output: completed
            .billed_output
            .saturating_add(current.billed_output),
        reasoning_detail: completed
            .reasoning_detail
            .saturating_add(current.reasoning_detail),
        reasoning_accounting,
        cache_status: combine_status(completed.cache_status, current.cache_status),
        cache_write_status: combine_status(
            completed.cache_write_status,
            current.cache_write_status,
        ),
        cache_write_ttl_status: combine_status(
            completed.cache_write_ttl_status,
            current.cache_write_ttl_status,
        ),
        cache_telemetry_input: completed
            .cache_telemetry_input
            .saturating_add(current.cache_telemetry_input),
        explicit_cache_storage_token_hours: completed
            .explicit_cache_storage_token_hours
            .zip(current.explicit_cache_storage_token_hours)
            .map(|(left, right)| left + right),
    }
}

fn cumulative_cache_cost(
    completed: Option<CacheCostEstimate>,
    current: Option<CacheCostEstimate>,
) -> Option<CacheCostEstimate> {
    completed
        .zip(current)
        .map(|(left, right)| CacheCostEstimate {
            input_with_cache_usd: left.input_with_cache_usd + right.input_with_cache_usd,
            input_without_cache_usd: left.input_without_cache_usd + right.input_without_cache_usd,
            estimated_savings_usd: left.estimated_savings_usd + right.estimated_savings_usd,
            explicit_storage_usd: left.explicit_storage_usd + right.explicit_storage_usd,
        })
}

fn cumulative_usage(completed: Option<&Usage>, current: &Usage) -> Result<Usage, DriveError> {
    let Some(completed) = completed else {
        return Ok(current.clone());
    };
    let mut accounts = completed.accounts.clone();
    for current_account in &current.accounts {
        if let Some(total) = accounts
            .iter_mut()
            .find(|total| total.account == current_account.account)
        {
            total.input = total.input.saturating_add(current_account.input);
            total.output = total.output.saturating_add(current_account.output);
            total.reasoning = total.reasoning.saturating_add(current_account.reasoning);
            total.cached = total.cached.saturating_add(current_account.cached);
            total.source = current_account.source;
            total.normalized = total
                .normalized
                .as_ref()
                .zip(current_account.normalized.as_ref())
                .map(|(left, right)| cumulative_normalized(left, right));
            total.cache_cost = cumulative_cache_cost(total.cache_cost, current_account.cache_cost);
            total.scope = current_account.scope.clone();
        } else {
            accounts.push(current_account.clone());
        }
    }
    let account = match accounts.as_slice() {
        [only] => Some(only.account.clone()),
        [] if completed.account == current.account => current.account.clone(),
        _ => None,
    };
    Ok(Usage {
        // Usage is accounting telemetry, not a reason to rewrite an otherwise
        // successful turn into Errored. Saturation preserves monotonic
        // cumulative snapshots at the protocol's representable maximum.
        input: completed.input.saturating_add(current.input),
        output: completed.output.saturating_add(current.output),
        reasoning: completed.reasoning.saturating_add(current.reasoning),
        cached: completed.cached.saturating_add(current.cached),
        source: current.source,
        account,
        accounts,
        normalized: completed
            .normalized
            .as_ref()
            .zip(current.normalized.as_ref())
            .map(|(left, right)| cumulative_normalized(left, right)),
        scope: current.scope.clone(),
        cache_cost: cumulative_cache_cost(completed.cache_cost, current.cache_cost),
        request: current.request.clone(),
    })
}

fn finalize_request_usage(
    completed: &mut Option<Usage>,
    current: &mut Option<Usage>,
) -> Result<(), DriveError> {
    if let Some(usage) = current.take() {
        *completed = Some(cumulative_usage(completed.as_ref(), &usage)?);
    }
    Ok(())
}

/// Daemon/core context threshold policy. Wire clients consume the emitted
/// threshold and must not recalculate it locally.
#[must_use]
pub fn context_soft_threshold_tokens(window: u64, reserved_output_tokens: u64) -> Option<u64> {
    context_tier_threshold_tokens(window, reserved_output_tokens, CONTEXT_SUMMARY_TIER_PERCENT)
}

#[must_use]
pub fn context_tier_threshold_tokens(
    window: u64,
    reserved_output_tokens: u64,
    percent: u64,
) -> Option<u64> {
    let hard_fit = window.checked_sub(reserved_output_tokens)?;
    let percentage = u64::try_from(u128::from(window).saturating_mul(u128::from(percent)) / 100)
        .unwrap_or(u64::MAX);
    Some(percentage.min(hard_fit))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuralTrimOutcome {
    removed_messages_before_protected_start: usize,
    removed_pairs: usize,
    removed_tool_call_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MessageBlockCoordinate {
    message: usize,
    block: usize,
}

/// Drops complete, unambiguous stale tool-call/result pairs while retaining
/// every other block byte-for-byte. Current-turn pairs and image-bearing
/// results are protected regardless of the requested pair count.
fn trim_stale_tool_pairs(
    messages: &mut Vec<Message>,
    protected_start: usize,
    retained_pairs: usize,
) -> StructuralTrimOutcome {
    let mut calls = HashMap::<&str, Vec<MessageBlockCoordinate>>::new();
    let mut results = HashMap::<&str, Vec<(MessageBlockCoordinate, bool)>>::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.blocks.iter().enumerate() {
            match block {
                Block::ToolCall { call_id, .. } => {
                    calls
                        .entry(call_id)
                        .or_default()
                        .push(MessageBlockCoordinate {
                            message: message_index,
                            block: block_index,
                        })
                }
                Block::ToolResult {
                    call_id, images, ..
                } => results.entry(call_id).or_default().push((
                    MessageBlockCoordinate {
                        message: message_index,
                        block: block_index,
                    },
                    !images.is_empty(),
                )),
                _ => {}
            }
        }
    }
    let mut pairs = calls
        .into_iter()
        .filter_map(|(call_id, call_positions)| {
            let result_positions = results.get(call_id)?;
            let ([call], [(result, has_images)]) =
                (call_positions.as_slice(), result_positions.as_slice())
            else {
                return None;
            };
            (call.message < result.message).then_some((
                call_id.to_owned(),
                *call,
                *result,
                *has_images,
            ))
        })
        .collect::<Vec<_>>();
    pairs.sort_unstable_by_key(|(_, call, _, _)| (call.message, call.block));
    // Current-turn and image-bearing pairs are independently protected. They
    // do not consume the stale-pair retention quota.
    let eligible_pairs = pairs
        .iter()
        .filter(|(_, call, result, has_images)| {
            !has_images && call.message < protected_start && result.message < protected_start
        })
        .count();
    let remove_count = eligible_pairs.saturating_sub(retained_pairs);
    let mut remove = HashSet::<MessageBlockCoordinate>::new();
    let mut removed_pairs = 0_usize;
    let mut removed_tool_call_ids = Vec::new();
    for (call_id, call, result, has_images) in pairs {
        if has_images || call.message >= protected_start || result.message >= protected_start {
            continue;
        }
        if removed_pairs >= remove_count {
            break;
        }
        remove.insert(call);
        remove.insert(result);
        removed_tool_call_ids.push(call_id);
        removed_pairs = removed_pairs.saturating_add(1);
    }
    if remove.is_empty() {
        return StructuralTrimOutcome {
            removed_messages_before_protected_start: 0,
            removed_pairs: 0,
            removed_tool_call_ids: Vec::new(),
        };
    }

    let mut removed_messages_before_protected_start = 0_usize;
    let mut message_index = 0_usize;
    messages.retain_mut(|message| {
        let original_message_index = message_index;
        message_index = message_index.saturating_add(1);
        let mut block_index = 0_usize;
        message.blocks.retain(|_| {
            let coordinate = MessageBlockCoordinate {
                message: original_message_index,
                block: block_index,
            };
            block_index = block_index.saturating_add(1);
            !remove.contains(&coordinate)
        });
        let retain = !message.blocks.is_empty();
        if !retain && original_message_index < protected_start {
            removed_messages_before_protected_start =
                removed_messages_before_protected_start.saturating_add(1);
        }
        retain
    });
    removed_tool_call_ids.sort_unstable();
    removed_tool_call_ids.dedup();
    StructuralTrimOutcome {
        removed_messages_before_protected_start,
        removed_pairs,
        removed_tool_call_ids,
    }
}

fn context_accounting(config: &HarnessConfig, used_tokens: u64, window: u64) -> ContextAccounting {
    build_context_accounting(
        &config.context_economy,
        config.structural_context_trimming,
        used_tokens,
        window,
        config.reserved_output_tokens,
    )
}

/// Builds the additive machine-readable context meter used by live and idle
/// compaction paths. Clients consume these daemon-owned coordinates instead
/// of duplicating threshold policy.
#[must_use]
pub fn build_context_accounting(
    economy: &ContextEconomy,
    structural_context_trimming: bool,
    used_tokens: u64,
    window: u64,
    reserved_output_tokens: u64,
) -> ContextAccounting {
    let usage_basis_points =
        u32::try_from(u128::from(used_tokens).saturating_mul(10_000) / u128::from(window.max(1)))
            .unwrap_or(u32::MAX);
    let tier_specs = if structural_context_trimming {
        [
            Some((
                ContextCompactionTier::StructuralTrim24,
                CONTEXT_STRUCTURAL_TIER_ONE_PERCENT,
            )),
            Some((
                ContextCompactionTier::StructuralTrim12,
                CONTEXT_STRUCTURAL_TIER_TWO_PERCENT,
            )),
            Some((
                ContextCompactionTier::Summarize,
                CONTEXT_SUMMARY_TIER_PERCENT,
            )),
        ]
    } else {
        [
            Some((
                ContextCompactionTier::Summarize,
                CONTEXT_SUMMARY_TIER_PERCENT,
            )),
            None,
            None,
        ]
    };
    let mut resolved = tier_specs
        .into_iter()
        .flatten()
        .filter_map(|(tier, percent)| {
            context_tier_threshold_tokens(window, reserved_output_tokens, percent)
                .map(|threshold| (tier, threshold))
        });
    let next = resolved
        .find(|(_, threshold)| used_tokens < *threshold)
        .or_else(|| {
            context_soft_threshold_tokens(window, reserved_output_tokens)
                .map(|threshold| (ContextCompactionTier::Summarize, threshold))
        });
    ContextAccounting {
        used_tokens,
        model_limit_tokens: window,
        remaining_tokens: window.saturating_sub(used_tokens),
        usage_basis_points,
        next_tier: next.map(|(tier, _)| tier),
        next_tier_at_tokens: next.map(|(_, threshold)| threshold),
        tokens_until_next_tier: next.map(|(_, threshold)| threshold.saturating_sub(used_tokens)),
        economy: economy.clone(),
    }
}

/// Returns whether one compaction failed the hard-fit or minimum-effectiveness
/// laws. Integer cross-multiplication keeps the exact 15% boundary stable for
/// small footprints and avoids floating-point rounding.
#[must_use]
pub fn compaction_guard_tripped(before: u64, after: u64, input_budget: u64) -> bool {
    if after > input_budget {
        return true;
    }
    let freed = before.saturating_sub(after);
    u128::from(freed).saturating_mul(100)
        < u128::from(before).saturating_mul(u128::from(COMPACTION_MIN_FREED_PERCENT))
}

fn estimated_context_footprint(config: &HarnessConfig, messages: &[Message]) -> ContextFootprint {
    context_footprint(
        config,
        estimated_request_input_tokens(config, messages),
        0,
        0,
        ContextFootprintTruth::Estimated,
    )
}

fn estimated_request_shaped_context_footprint(
    config: &HarnessConfig,
    messages: &[Message],
    volatile_user_tail: Option<&str>,
) -> ContextFootprint {
    let Some(tail) = volatile_user_tail else {
        return estimated_context_footprint(config, messages);
    };
    let mut measured = messages.to_vec();
    measured.push(Message::user_text(tail));
    estimated_context_footprint(config, &measured)
}

fn estimated_request_savings_tokens(
    config: &HarnessConfig,
    messages: &[Message],
    volatile_user_tail: Option<&str>,
) -> u64 {
    let mut projection = messages.to_vec();
    if let Some(tail) = volatile_user_tail {
        projection.push(Message::user_text(tail));
    }
    apply_tool_result_image_budget(&mut projection);
    if !config.tool_result_images_supported {
        degrade_tool_result_images_to_placeholders(&mut projection);
    }
    estimate_provider_request_bytes_div_four_raw(
        &projection,
        &config.system_prompt,
        config.tool_definitions(),
    )
}

fn context_footprint_from_usage(
    config: &HarnessConfig,
    usage: &Usage,
    messages: &[Message],
) -> ContextFootprint {
    if usage.source != haider_protocol::provider::UsageSource::ProviderReported {
        return estimated_context_footprint(config, messages);
    }
    if let Some(normalized) = &usage.normalized {
        return context_footprint(
            config,
            normalized.uncached_input,
            normalized.billed_output,
            normalized.cache_read_input,
            ContextFootprintTruth::Exact,
        );
    }
    let input_tokens = if config.cached_input_is_subset {
        let Some(uncached) = usage.input.checked_sub(usage.cached) else {
            return estimated_context_footprint(config, messages);
        };
        uncached
    } else {
        usage.input
    };
    context_footprint(
        config,
        input_tokens,
        usage.output,
        usage.cached,
        ContextFootprintTruth::Exact,
    )
}

fn context_footprint(
    config: &HarnessConfig,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    truth: ContextFootprintTruth,
) -> ContextFootprint {
    let used_tokens = input_tokens
        .saturating_add(output_tokens)
        .saturating_add(cached_input_tokens);
    let soft_threshold_tokens = config
        .context_window
        .and_then(|window| context_soft_threshold_tokens(window, config.reserved_output_tokens));
    let estimated_turns_to_threshold = soft_threshold_tokens.and_then(|threshold| {
        if used_tokens >= threshold {
            return Some(0);
        }
        (output_tokens > 0).then(|| {
            let remaining = threshold.saturating_sub(used_tokens);
            remaining.saturating_add(output_tokens.saturating_sub(1)) / output_tokens
        })
    });
    ContextFootprint {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        used_tokens,
        context_window: config.context_window,
        reserved_output_tokens: config.reserved_output_tokens,
        soft_threshold_tokens,
        estimated_turns_to_threshold,
        truth,
        accounting: config
            .context_window
            .map(|window| context_accounting(config, used_tokens, window)),
    }
}

/// Conservative provider-neutral compiled-request accounting used when no
/// provider-reported request-local usage is available. It remains separate
/// from cumulative billing usage.
fn estimated_request_input_tokens(config: &HarnessConfig, messages: &[Message]) -> u64 {
    estimated_request_input_measure(config, messages).tokens
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestInputMeasure {
    bytes: u64,
    tokens: u64,
}

fn estimated_request_input_measure(
    config: &HarnessConfig,
    messages: &[Message],
) -> RequestInputMeasure {
    let mut projection = messages.to_vec();
    apply_tool_result_image_budget(&mut projection);
    if !config.tool_result_images_supported {
        degrade_tool_result_images_to_placeholders(&mut projection);
    }
    estimate_provider_request_input_measure_raw(
        &projection,
        &config.system_prompt,
        config.tool_definitions(),
        &config.attachments,
    )
}

/// Deterministic provider-neutral accounting for the textual request
/// projection, native-document payload bytes, and a per-image heuristic.
/// Daemon-owned idle operations use the same estimator as live actor rounds.
/// Image transport bytes remain excluded because their fixed visual-token
/// approximation is provider-neutral; native PDFs have no such alternate
/// measurement, so their resolved base64 request bytes bind the projection.
pub const VISION_IMAGE_ESTIMATE_TOKENS: u64 = 1_600;

#[must_use]
pub fn estimate_provider_request_input_tokens(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
    attachments: &[ResolvedAttachment],
) -> u64 {
    let mut projection = messages.to_vec();
    apply_tool_result_image_budget(&mut projection);
    estimate_provider_request_input_tokens_raw(&projection, system_prompt, tools, attachments)
}

/// The sole savings unit: `ceil(serialized provider-neutral request bytes/4)`.
/// Unlike context occupancy, this deliberately adds no image-token heuristic.
#[must_use]
pub fn estimate_provider_request_bytes_div_four(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
) -> u64 {
    let mut projection = messages.to_vec();
    apply_tool_result_image_budget(&mut projection);
    estimate_provider_request_bytes_div_four_raw(&projection, system_prompt, tools)
}

fn estimate_provider_request_bytes_div_four_raw(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
) -> u64 {
    provider_neutral_json_len(messages, system_prompt, tools)
        .map(|bytes| bytes.saturating_add(3) / 4)
        .unwrap_or(u64::MAX)
}

fn estimate_provider_request_input_tokens_raw(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
    attachments: &[ResolvedAttachment],
) -> u64 {
    estimate_provider_request_input_measure_raw(messages, system_prompt, tools, attachments).tokens
}

fn estimate_provider_request_input_measure_raw(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
    attachments: &[ResolvedAttachment],
) -> RequestInputMeasure {
    let textual_bytes =
        provider_neutral_json_len(messages, system_prompt, tools).unwrap_or(u64::MAX);
    let native_pdf_bytes = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| {
            let haider_protocol::provider::Block::Attachment(
                haider_protocol::tool::AttachmentBlock::Pdf {
                    artifact,
                    delivery: haider_protocol::tool::PdfDeliveryMode::NativeDocument,
                    ..
                },
            ) = block
            else {
                return None;
            };
            attachments
                .iter()
                .find(|attachment| &attachment.artifact == artifact)
        })
        .map(|attachment| u64::try_from(attachment.data_base64.len()).unwrap_or(u64::MAX))
        .fold(0_u64, u64::saturating_add);
    let bytes = textual_bytes.saturating_add(native_pdf_bytes);
    let image_count = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .map(|block| match block {
            haider_protocol::provider::Block::Attachment(
                haider_protocol::tool::AttachmentBlock::Image { .. },
            ) => 1,
            haider_protocol::provider::Block::ToolResult { images, .. } => images.len(),
            _ => 0,
        })
        .fold(0_usize, usize::saturating_add);
    let image_tokens = u64::try_from(image_count)
        .unwrap_or(u64::MAX)
        .saturating_mul(VISION_IMAGE_ESTIMATE_TOKENS);
    RequestInputMeasure {
        bytes,
        tokens: (bytes.saturating_add(3) / 4).saturating_add(image_tokens),
    }
}

/// Counts the exact provider-neutral JSON projection while replacing reply
/// ranges with tiny placeholders. The correction is computed from JSON escape
/// widths, so this is byte-identical to serde_json without allocating a
/// prompt-sized buffer or flattening a segmented reply.
fn provider_neutral_json_len(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
) -> Option<u64> {
    const MARKER: &str = "__haider_reply_range__";

    #[derive(Default)]
    struct CountingWriter(u64);
    impl std::io::Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| std::io::Error::other("request size overflow"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn json_string_len(text: &ReplyText) -> u64 {
        let mut len = 2_u64;
        text.visit_strs(|segment| {
            for byte in segment.bytes() {
                let width = match byte {
                    b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
                    0x00..=0x1f => 6,
                    _ => 1,
                };
                len = len.saturating_add(width);
            }
        });
        len
    }

    let marker_len = u64::try_from(MARKER.len()).ok()?.checked_add(2)?;
    let marker_text = ReplyText::from(MARKER);
    let mut adjusted = messages.to_vec();
    let mut correction = 0_i128;
    for message in &mut adjusted {
        for block in &mut message.blocks {
            if let Block::Text { text } = block {
                correction = correction
                    .checked_add(i128::from(json_string_len(text)))?
                    .checked_sub(i128::from(marker_len))?;
                *text = marker_text.clone();
            }
        }
    }
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, &(&adjusted, system_prompt, tools)).ok()?;
    let total = i128::from(counter.0).checked_add(correction)?;
    u64::try_from(total).ok()
}

/// Evaluates the request-local budget projection only when the caller has a
/// guard that can consume it. Keeping the guard check at the original
/// pre-prepare seam preserves capped-run decisions while making ordinary runs
/// clone- and serialization-free for this estimate.
fn estimate_if_budget_guarded<G: ?Sized>(
    guard: Option<&G>,
    estimate: impl FnOnce() -> u64,
) -> Option<u64> {
    guard.map(|_| estimate())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod usage_tests {
    use super::*;
    use haider_protocol::provider::UsageSource;

    #[test]
    fn unbudgeted_provider_request_skips_projection_estimator() {
        let calls = std::cell::Cell::new(0_u8);
        let projected = estimate_if_budget_guarded(None::<&()>, || {
            calls.set(calls.get().saturating_add(1));
            17
        });

        assert_eq!(projected, None);
        assert_eq!(calls.get(), 0);

        let projected = estimate_if_budget_guarded(Some(&()), || {
            calls.set(calls.get().saturating_add(1));
            17
        });
        assert_eq!(projected, Some(17));
        assert_eq!(calls.get(), 1);
    }

    fn diagnostic_digests(system: &str, tools: &str, history: &str) -> PrefixDigests {
        PrefixDigests {
            system: system.into(),
            tools: tools.into(),
            immutable_history: history.into(),
            model: "model-digest".into(),
            auth_mode: "auth-digest".into(),
            reasoning_settings: "reasoning-digest".into(),
        }
    }

    fn cache_miss_usage() -> NormalizedUsage {
        NormalizedUsage {
            logical_input: 4_096,
            uncached_input: 4_096,
            cache_read_input: 0,
            cache_write_input: 0,
            billed_output: 10,
            cache_status: CacheStatAvailability::Present,
            cache_telemetry_input: 4_096,
            ..NormalizedUsage::default()
        }
    }

    fn diagnostic_domain(key: &CacheDiagnosticKey, epoch: &str) -> Option<String> {
        Some(keyed_breakpoint_hash(key, "cache-domain", &[epoch]))
    }

    #[test]
    fn cache_diagnostic_stable_prefix_has_identical_keyed_breakpoints() {
        let key = CacheDiagnosticKey::from_bytes([0x5a; 32]);
        let digests = diagnostic_digests("system-a", "tools-a", "history-a");
        let first = cache_breakpoint_hashes(&key, &digests);
        let second = cache_breakpoint_hashes(&key, &digests);
        assert_eq!(first, second, "stable requests must have stable hashes");

        // Independent spelling of the v1 keyed-hash contract pins its domain
        // separation, component order, and length framing.
        let mut expected = blake3::Hasher::new_keyed(&[0x5a; 32]);
        expected.update(b"haider.cache-request-diagnostic.v1\0");
        expected.update(&("system".len() as u64).to_le_bytes());
        expected.update(b"system");
        expected.update(&("system-a".len() as u64).to_le_bytes());
        expected.update(b"system-a");
        assert_eq!(
            first.system,
            format!("blake3-keyed:{}", expected.finalize().to_hex())
        );
    }

    #[test]
    fn cache_diagnostic_changed_system_is_first_differing_breakpoint() {
        let key = CacheDiagnosticKey::from_bytes([0x31; 32]);
        let original = diagnostic_digests("system-a", "tools-a", "history-a");
        let changed = diagnostic_digests("system-b", "tools-a", "history-a");
        let previous = PreviousCacheRequest {
            history_message_count: 1,
            breakpoint_hashes: cache_breakpoint_hashes(&key, &original),
            cache_domain_hash: diagnostic_domain(&key, "epoch-a"),
        };
        let diagnostic = build_cache_request_diagnostic(
            &key,
            "custom",
            "model",
            "epoch-a",
            &changed,
            Some(&changed),
            Some(&previous),
            1,
            4_096,
            Some(1_000),
            CacheControlObservationV1::Emitted {
                ttl_ms: Some(300_000),
            },
            None,
        );
        assert_eq!(
            diagnostic.prefix_match,
            CachePrefixMatchV1::Changed {
                first: CacheBreakpointV1::System
            }
        );
        assert_eq!(
            classify_cache_request(&diagnostic, Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::PrefixChanged {
                first: CacheBreakpointV1::System
            })
        );
    }

    #[test]
    fn cache_diagnostic_old_length_proves_grown_prefix_contains_previous_entry() {
        let key = CacheDiagnosticKey::from_bytes([0x77; 32]);
        let previous_digests = diagnostic_digests("system-a", "tools-a", "history-one");
        let grown_digests = diagnostic_digests("system-a", "tools-a", "history-two");
        let previous = PreviousCacheRequest {
            history_message_count: 1,
            breakpoint_hashes: cache_breakpoint_hashes(&key, &previous_digests),
            cache_domain_hash: diagnostic_domain(&key, "epoch-a"),
        };
        let diagnostic = build_cache_request_diagnostic(
            &key,
            "custom",
            "model",
            "epoch-a",
            &grown_digests,
            Some(&previous_digests),
            Some(&previous),
            2,
            4_096,
            Some(1_000),
            CacheControlObservationV1::Emitted {
                ttl_ms: Some(300_000),
            },
            None,
        );
        let old = diagnostic
            .previous_breakpoint
            .as_ref()
            .expect("previous moving boundary is recorded");
        assert_eq!(old.message_count, 1);
        assert_eq!(old.actual_hash.as_ref(), Some(&old.expected_hash));
        assert_eq!(diagnostic.prefix_match, CachePrefixMatchV1::Same);
        assert_ne!(
            diagnostic.breakpoint_hashes.history, previous.breakpoint_hashes.history,
            "the current moving breakpoint is expected to advance"
        );
    }

    #[test]
    fn cache_diagnostic_records_never_contain_prompt_or_secret_content() {
        const SYSTEM_SECRET: &str = "system-secret-never-journal";
        const TOOL_SECRET: &str = "tool-secret-never-journal";
        const ARG_SECRET: &str = "argument-secret-never-journal";
        const USER_SECRET: &str = "user-secret-never-journal";

        let mut config = HarnessConfig::for_session(
            SessionId::new("privacy-session"),
            DeviceId::new("privacy-device"),
            0,
            0,
        );
        config.system_prompt = Some(SYSTEM_SECRET.into());
        config.tools = vec![ToolDefinition {
            name: "private_tool".into(),
            description: TOOL_SECRET.into(),
            input_schema: serde_json::json!({"type":"object"}),
        }];
        let history = vec![
            Message::user_text(USER_SECRET),
            Message::assistant(vec![Block::ToolCall {
                call_id: "private-call".into(),
                name: "private_tool".into(),
                args: serde_json::json!({"secret": ARG_SECRET}),
            }]),
        ];
        let diagnostic = build_cache_request_diagnostic(
            &config.cache_diagnostic_key,
            "custom",
            "model",
            "epoch-a",
            &usage_prefix_digests(&config, &history),
            None,
            None,
            history.len(),
            123,
            None,
            CacheControlObservationV1::Unavailable,
            None,
        );
        let attempt = CacheRequestAttemptV1 {
            ordinal: 1,
            correlation: None,
            diagnostic: diagnostic.clone(),
        }
        .extension_item()
        .expect("attempt serializes");
        let response = RequestUsage {
            ordinal: 1,
            input: 10,
            output: 2,
            reasoning: None,
            cached: None,
            source: UsageSource::ProviderReported,
            account: None,
            normalized: None,
            cache_cost: None,
            cache: Some(diagnostic),
        };
        let records = serde_json::to_string(&(attempt, response)).expect("records serialize");
        for secret in [SYSTEM_SECRET, TOOL_SECRET, ARG_SECRET, USER_SECRET] {
            assert!(!records.contains(secret), "record leaked {secret}");
        }
        assert!(
            !records.contains("\"normalized\"")
                && !records.contains("\"cache_cost\"")
                && !records.contains("\"reasoning\"")
                && !records.contains("\"cached\"")
                && !records.contains("\"classification\""),
            "unmeasured optional telemetry must stay absent: {records}"
        );
    }

    #[test]
    fn cache_diagnostic_planned_rewarm_is_distinct_from_unexpected_miss() {
        let key = CacheDiagnosticKey::from_bytes([0x22; 32]);
        let digests = diagnostic_digests("system-a", "tools-a", "history-a");
        let previous = PreviousCacheRequest {
            history_message_count: 1,
            breakpoint_hashes: cache_breakpoint_hashes(&key, &digests),
            cache_domain_hash: diagnostic_domain(&key, "epoch-a"),
        };
        let build = |rewarm| {
            build_cache_request_diagnostic(
                &key,
                "custom",
                "model",
                "epoch-a",
                &digests,
                Some(&digests),
                Some(&previous),
                1,
                4_096,
                Some(1_000),
                CacheControlObservationV1::Emitted {
                    ttl_ms: Some(300_000),
                },
                rewarm,
            )
        };
        assert_eq!(
            classify_cache_request(&build(None), Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::SamePrefixInTtl)
        );
        assert_eq!(
            classify_cache_request(
                &build(Some(CacheRewarmReasonV1::PlannedCompaction)),
                Some(&cache_miss_usage())
            ),
            Some(CacheMissClassificationV1::PlannedCompaction)
        );
        let changed_domain = build_cache_request_diagnostic(
            &key,
            "custom",
            "model",
            "epoch-b",
            &digests,
            Some(&digests),
            Some(&previous),
            1,
            4_096,
            Some(1_000),
            CacheControlObservationV1::Emitted {
                ttl_ms: Some(300_000),
            },
            None,
        );
        assert_eq!(
            classify_cache_request(&changed_domain, Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::ConfigurationChange)
        );

        let mut below_minimum = build(None);
        below_minimum.cacheable_minimum_tokens = Some(8_192);
        assert_eq!(
            classify_cache_request(&below_minimum, Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::BelowMinimum)
        );

        let mut control_missing = build(None);
        control_missing.control = CacheControlObservationV1::NotEmitted {
            reason: haider_protocol::provider::CacheControlOmissionReasonV1::UnsupportedModel,
        };
        assert_eq!(
            classify_cache_request(&control_missing, Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::ControlNotEmitted {
                reason: haider_protocol::provider::CacheControlOmissionReasonV1::UnsupportedModel
            })
        );

        let mut expired = build(None);
        expired.reuse_gap_ms = Some(300_001);
        assert_eq!(
            classify_cache_request(&expired, Some(&cache_miss_usage())),
            Some(CacheMissClassificationV1::Expired)
        );

        let mut hit = cache_miss_usage();
        hit.cache_read_input = 1;
        assert_eq!(classify_cache_request(&build(None), Some(&hit)), None);
        assert_eq!(
            classify_cache_request(&build(None), None),
            Some(CacheMissClassificationV1::Unavailable)
        );
    }

    #[test]
    fn cache_diagnostic_record_size_and_cpu_cost_are_bounded() {
        let key = CacheDiagnosticKey::from_bytes([0x44; 32]);
        let digests = diagnostic_digests("system-a", "tools-a", "history-a");
        let previous = PreviousCacheRequest {
            history_message_count: 42,
            breakpoint_hashes: cache_breakpoint_hashes(&key, &digests),
            cache_domain_hash: diagnostic_domain(&key, "epoch-a"),
        };
        let mut latencies = Vec::with_capacity(10_000);
        let mut last_size = 0_usize;
        for _ in 0..10_000 {
            let started = std::time::Instant::now();
            let diagnostic = build_cache_request_diagnostic(
                &key,
                "openai",
                "gpt-5.6-terra",
                "epoch-a",
                &digests,
                Some(&digests),
                Some(&previous),
                43,
                4_096,
                Some(1_000),
                CacheControlObservationV1::Emitted {
                    ttl_ms: Some(300_000),
                },
                None,
            );
            last_size = serde_json::to_vec(&CacheRequestAttemptV1 {
                ordinal: 1,
                correlation: None,
                diagnostic,
            })
            .expect("diagnostic serializes")
            .len();
            latencies.push(started.elapsed());
        }
        latencies.sort_unstable();
        let total_nanos = latencies
            .iter()
            .map(std::time::Duration::as_nanos)
            .sum::<u128>();
        let mean_nanos = total_nanos / latencies.len() as u128;
        let p95_nanos = latencies[latencies.len() * 95 / 100].as_nanos();
        eprintln!(
            "cache diagnostic CPU: bytes={last_size} mean_ns={mean_nanos} p95_ns={p95_nanos} samples={}",
            latencies.len()
        );
        assert!(
            last_size <= 1_024,
            "attempt record grew to {last_size} bytes"
        );
    }

    #[test]
    fn cumulative_usage_saturates_each_counter_without_failing_the_turn() {
        let completed = Usage {
            input: u64::MAX,
            output: u64::MAX - 1,
            reasoning: u64::MAX - 2,
            cached: u64::MAX - 3,
            source: UsageSource::ProviderReported,
            account: None,
            accounts: Vec::new(),
            normalized: None,
            scope: None,
            cache_cost: None,
            request: None,
        };
        let current = Usage {
            input: 1,
            output: 2,
            reasoning: 3,
            cached: 4,
            source: UsageSource::ProviderReported,
            account: None,
            accounts: Vec::new(),
            normalized: None,
            scope: None,
            cache_cost: None,
            request: None,
        };
        let cumulative = cumulative_usage(Some(&completed), &current).expect("same account");
        assert_eq!(cumulative.input, u64::MAX);
        assert_eq!(cumulative.output, u64::MAX);
        assert_eq!(cumulative.reasoning, u64::MAX);
        assert_eq!(cumulative.cached, u64::MAX);
    }

    /// MUTATION CHECK: replace the predicate loop in `fired` with one
    /// notification await. Expected runtime failure: the stored permit from
    /// the first pre-poll wake resolves the newly armed second event.
    #[tokio::test]
    async fn provider_retry_wake_discards_a_stale_notify_permit_between_events() {
        let wake = ProviderRetryWake::default();
        let first = EventId::new("retrying-first");
        wake.arm(first.clone());
        assert!(wake.wake("wake-first".into(), &first));
        // Constructing `fired` only after the wake makes its fast predicate
        // path leave Notify's stored permit untouched.
        wake.fired(&first).await;
        wake.disarm(&first);

        let second = EventId::new("retrying-second");
        wake.arm(second.clone());
        let pending = wake.fired(&second);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            () = &mut pending => panic!("stale permit resolved a different retry event"),
            () = tokio::task::yield_now() => {}
        }
        assert!(wake.wake("wake-second".into(), &second));
        pending.await;
        wake.disarm(&second);
    }

    /// CM2a — no-op session configuration keeps system/tool/model/auth/
    /// reasoning digests stable while immutable history grows append-only.
    ///
    /// MUTATION CHECK (executed): salt the tool digest per request or hash
    /// history into it; the successive equality assertion fails.
    #[test]
    fn cm2a_system_and_tool_digests_are_stable_across_append_only_history() {
        let mut config = HarnessConfig::for_session(
            SessionId::new("digest-session"),
            DeviceId::new("digest-device"),
            0,
            0,
        );
        config.model = "gpt-5.6-terra".into();
        config.system_prompt = Some("stable system".into());
        config.tools = vec![ToolDefinition {
            name: "read".into(),
            description: "stable tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        config.usage_scope.auth_scope = "api_key".into();
        config.reasoning_settings = r#"{"effort":"high","fast":false}"#.into();
        let first_history = vec![Message::user_text("first")];
        let mut second_history = first_history.clone();
        second_history.push(Message::assistant(vec![Block::Text {
            text: "answer".into(),
        }]));
        let first = usage_prefix_digests(&config, &first_history);
        let second = usage_prefix_digests(&config, &second_history);
        assert_eq!(first.system, second.system);
        assert_eq!(first.tools, second.tools);
        assert_eq!(first.model, second.model);
        assert_eq!(first.auth_mode, second.auth_mode);
        assert_eq!(first.reasoning_settings, second.reasoning_settings);
        assert_ne!(
            first.immutable_history, second.immutable_history,
            "append-only history has its own diagnostic digest"
        );

        // Executed mutation: perturb each owned digest input independently.
        // Omitting either input from the implementation kills these checks.
        let mut changed_system = config.clone();
        changed_system.system_prompt = Some("mutated system".into());
        assert_ne!(
            first.system,
            usage_prefix_digests(&changed_system, &first_history).system
        );
        let mut changed_tools = config;
        changed_tools.tools[0].description.push_str(" mutated");
        assert_ne!(
            first.tools,
            usage_prefix_digests(&changed_tools, &first_history).tools
        );

        // Haider owns tool schemas, so key insertion order is canonicalized
        // for the diagnostic/cache-domain digest without changing wire bytes.
        let mut left_schema = serde_json::Map::new();
        left_schema.insert("zeta".into(), serde_json::json!({"type": "string"}));
        left_schema.insert("alpha".into(), serde_json::json!({"type": "number"}));
        let mut right_schema = serde_json::Map::new();
        right_schema.insert("alpha".into(), serde_json::json!({"type": "number"}));
        right_schema.insert("zeta".into(), serde_json::json!({"type": "string"}));
        let schema_tool = |input_schema| ToolDefinition {
            name: "canonical".into(),
            description: "canonical".into(),
            input_schema,
        };
        assert_eq!(
            canonical_tool_definitions_digest(&[schema_tool(serde_json::Value::Object(
                left_schema
            ))]),
            canonical_tool_definitions_digest(&[schema_tool(serde_json::Value::Object(
                right_schema
            ))])
        );
    }
}

/// Bounds and de-controls a message destined for a durable `RunFailed`
/// payload (R3, authoritative site — daemon writers delegate here).
///
/// The durable failure record must be safe to journal and render: no more
/// characters are accepted once 512 bytes have accumulated (the final
/// accepted `char` may carry the total a few bytes past the limit — the
/// bound is hard at 515), and control characters other than `\n` become
/// spaces. Provider response bodies and secrets are never eligible as input
/// — callers pass only typed `HaiderError` messages.
pub fn sanitized_failure_message(message: &str) -> String {
    const LIMIT: usize = 512;
    let mut sanitized = String::with_capacity(message.len().min(LIMIT));
    for character in message.chars() {
        if sanitized.len() >= LIMIT {
            break;
        }
        sanitized.push(if character.is_control() && character != '\n' {
            ' '
        } else {
            character
        });
    }
    sanitized
}

fn tool_error_presentation(subcode: &str, title: &str, detail: &str) -> ErrorPresentation {
    ErrorPresentation::new(
        subcode,
        title,
        detail,
        ErrorScope::Tool,
        [ErrorAction::None],
    )
}

fn tool_image_corrupt(message: impl Into<String>) -> HaiderError {
    HaiderError::new(ErrorCode::StoreCorrupt, message, false)
}

/// E2 normalization point: every tool result passes through the actor before
/// it is journaled, so legacy dispatchers cannot accidentally omit the typed
/// presentation on a non-success result.
fn ensure_tool_result_presentation(result: &mut BoundedResult) {
    if result.status.is_completed() || result.presentation.is_some() {
        return;
    }
    let (subcode, title, detail, action) = match result.status {
        ToolResultStatus::Completed => return,
        ToolResultStatus::Rejected => (
            "tool-rejected",
            "Tool request rejected",
            "The tool request was not authorized.",
            ErrorAction::None,
        ),
        ToolResultStatus::Conflict => (
            "tool-conflict",
            "Tool request conflicted",
            "The tool could not safely apply because the target changed.",
            ErrorAction::Retry,
        ),
        ToolResultStatus::Failed => (
            "tool-failed",
            "Tool execution failed",
            "The tool did not complete successfully.",
            ErrorAction::Retry,
        ),
        ToolResultStatus::Cancelled => (
            "tool-cancelled",
            "Tool execution cancelled",
            "The tool stopped before it completed.",
            ErrorAction::Retry,
        ),
        ToolResultStatus::Unknown => (
            "tool-outcome-unknown",
            "Tool outcome unknown",
            "Haider could not confirm whether the tool completed.",
            ErrorAction::None,
        ),
    };
    result.presentation = Some(ErrorPresentation::new(
        subcode,
        title,
        detail,
        ErrorScope::Tool,
        [action],
    ));
}

fn drive_error_to_haider(error: DriveError) -> HaiderError {
    match error {
        DriveError::Provider(error) => provider_error_to_haider(error),
        DriveError::Account(error) | DriveError::Store(error) => error,
        DriveError::Cancelled => HaiderError::new(
            ErrorCode::Internal,
            "cancelled drive error escaped its turn outcome boundary",
            false,
        ),
    }
}

fn tool_error_to_drive(error: haider_tools::ToolError) -> DriveError {
    DriveError::Provider(provider_protocol_error(error.to_string()))
}

/// Terminal outcome for faults where even the `Errored` commit failed.
fn errored_outcome(error: HaiderError) -> TurnOutcome {
    TurnOutcome {
        state: RunState::Errored,
        finish_reason: FinishReason::Error,
        error: Some(error),
    }
}

/// UI + durable, omitted from prompt reconstruction (state/usage bookkeeping).
fn prompt_omit_render() -> RenderTargets {
    RenderTargets {
        ui: true,
        durable: true,
        prompt: PromptRender::Omit,
    }
}

/// UI + durable, replayed verbatim into future prompts (conversation content).
fn prompt_verbatim_render() -> RenderTargets {
    RenderTargets {
        ui: true,
        durable: true,
        prompt: PromptRender::Verbatim,
    }
}

/// Durable + prompt-verbatim, hidden from UI because encrypted continuation
/// state is provider machinery rather than user-visible reasoning.
fn hidden_prompt_verbatim_render() -> RenderTargets {
    RenderTargets {
        ui: false,
        durable: true,
        prompt: PromptRender::Verbatim,
    }
}

/// Durable + UI-hidden marker, excluded from provider prompt rendering.
fn hidden_prompt_omit_render() -> RenderTargets {
    RenderTargets {
        ui: false,
        durable: true,
        prompt: PromptRender::Omit,
    }
}

#[cfg(test)]
mod cu1_actor_tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::MemoryStore;
    use haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN;
    use haider_provider::{FakeProvider, Message};
    use std::collections::HashMap;

    struct ImageReaderFixture {
        objects: HashMap<ArtifactRef, Vec<u8>>,
    }

    #[async_trait]
    impl ArtifactReader for ImageReaderFixture {
        async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
            self.objects.get(artifact).cloned().ok_or_else(|| {
                HaiderError::new(ErrorCode::InvalidArgument, "missing fixture image", false)
            })
        }
    }

    fn actor_config(images_supported: bool) -> HarnessConfig {
        let mut config = HarnessConfig::for_session(
            SessionId::new("cu1-actor-unit"),
            DeviceId::new("cu1-device"),
            1,
            1,
        );
        config.tool_result_images_supported = images_supported;
        config
    }

    #[test]
    fn root_workflow_rejects_a_forged_unadvertised_actor_tool() {
        let mut config = actor_config(false);
        assert!(
            tool_call_within_advertised_ceiling(&config, "todo_write"),
            "ordinary root compatibility remains unchanged"
        );

        config.enforce_advertised_tool_ceiling = true;
        config.tools.push(ToolDefinition {
            name: "graph_evidence".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        assert!(tool_call_within_advertised_ceiling(
            &config,
            "graph_evidence"
        ));
        for forged in ["request_input", "plan", "todo_write"] {
            assert!(
                !tool_call_within_advertised_ceiling(&config, forged),
                "root Loom turn admitted forged {forged}"
            );
        }
    }

    #[test]
    fn malformed_tool_argument_buffer_is_a_typed_provider_failure() {
        let tool = ToolAccumulator {
            item_id: ItemId::new("item-malformed-args"),
            call_id: "call-malformed-args".into(),
            name: "shell".into(),
            args: r#"{"command":"#.into(),
            requested_name: None,
            parsed_args: OnceLock::new(),
        };
        let error = parse_tool_args(&tool).expect_err("truncated JSON must fail");
        let DriveError::Provider(error) = error else {
            panic!("tool JSON parse failure must retain provider classification")
        };
        assert_eq!(error.kind, ProviderErrorKind::MalformedFrame);
        assert_eq!(
            error.presentation.subcode.as_str(),
            "malformed-tool-arguments"
        );
        assert!(matches!(tool.parsed_args.get(), Some(Err(_))));
        assert_eq!(
            tool_args_or_raw(&tool),
            serde_json::Value::String(tool.args.clone())
        );
    }

    #[test]
    fn completed_tool_arguments_reuse_one_parsed_value() {
        let tool = ToolAccumulator {
            item_id: ItemId::new("item-cached-args"),
            call_id: "call-cached-args".into(),
            name: "shell".into(),
            args: r#"{"command":"pwd","nested":{"limit":2}}"#.into(),
            requested_name: None,
            parsed_args: OnceLock::new(),
        };

        let first = parse_tool_args(&tool).expect("initial parse");
        let second = parse_tool_args(&tool).expect("cached parse");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(tool_args_or_raw(&tool), first.as_ref().clone());
    }

    fn tool_definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} fixture"),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
            }),
        }
    }

    #[test]
    fn shared_pair_switch_retains_fallback_when_requested_web_tool_is_unauthorized() {
        let mut config = actor_config(false);
        let base: Arc<[ToolDefinition]> = vec![tool_definition("fs_read")].into();
        let digest = canonical_tool_definitions_digest(&base);
        let variants = HashMap::from([(Vec::new(), (Arc::clone(&base), digest.clone()))]);
        let initial = ProviderDerivedRequestState {
            tool_result_images_supported: false,
            local_web_tool_names: Vec::new(),
            provider_fallback_local_web_tool_names: Vec::new(),
        };
        config.install_shared_tool_packs(
            SharedToolPacks {
                base: Arc::clone(&base),
                local_web_tool_names: Arc::default(),
                current: Arc::clone(&base),
                current_digest: digest.clone(),
                fallback: None,
                variants: Arc::new(variants),
            },
            &initial,
        );

        config.install_provider_derived_request_state(&ProviderDerivedRequestState {
            tool_result_images_supported: false,
            local_web_tool_names: Vec::new(),
            provider_fallback_local_web_tool_names: vec!["web_fetch".into()],
        });

        assert!(config.has_provider_tool_fallback());
        config.activate_provider_tool_fallback();
        assert_eq!(config.tool_definitions(), base.as_ref());
        assert_eq!(config.canonical_tool_pack_digest(), digest);
    }

    #[test]
    fn standalone_mutable_tool_vectors_never_retain_cached_digests() {
        let mut config = actor_config(false);
        let fs_read = tool_definition("fs_read");
        let web_fetch = tool_definition("web_fetch");
        config.provider_tool_base = Some(vec![fs_read.clone(), web_fetch.clone()]);
        config.provider_local_web_tools = vec![web_fetch];
        config.install_provider_derived_request_state(&ProviderDerivedRequestState {
            tool_result_images_supported: false,
            local_web_tool_names: Vec::new(),
            provider_fallback_local_web_tool_names: vec!["web_fetch".into()],
        });

        assert!(config.tool_pack_digest.is_none());
        assert!(config.provider_tool_fallback_digest.is_none());
        config.tools.push(tool_definition("late_current"));
        assert_eq!(
            config.canonical_tool_pack_digest(),
            canonical_tool_definitions_digest(&config.tools)
        );

        config.activate_provider_tool_fallback();
        config.tools.push(tool_definition("late_fallback"));
        assert_eq!(
            config.canonical_tool_pack_digest(),
            canonical_tool_definitions_digest(&config.tools)
        );
    }

    #[test]
    fn transport_fault_classifier_is_narrow() {
        for kind in [
            ProviderErrorKind::Transport,
            ProviderErrorKind::StreamInterrupted,
        ] {
            assert!(provider_error_is_transport_fault(&ProviderError::new(
                kind,
                "transport fixture"
            )));
        }
        assert!(!provider_error_is_transport_fault(&ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "a response fault may retain explicit recovery"
        )));
    }

    #[test]
    fn provider_timeout_presentation_maps_to_existing_timeout_error_code() {
        let provider_error = ProviderError::new(ProviderErrorKind::Transport, "timed out")
            .with_presentation(ErrorPresentation::new(
                "provider-timeout",
                "Provider request timed out",
                "The local deadline expired.",
                ErrorScope::Turn,
                [ErrorAction::Retry],
            ))
            .with_timeout_budget(60_000, 60_000);
        let error = provider_error_to_haider(provider_error);
        assert_eq!(error.code, ErrorCode::ProviderTimeout);
        assert!(error.retryable);
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|value| value["opened_within_ms"].as_u64()),
            Some(60_000)
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|value| value["budget_ms"].as_u64()),
            Some(60_000)
        );
        let presentation = error.presentation.expect("provider presentation");
        assert_eq!(presentation.opened_within_ms, Some(60_000));
        assert_eq!(presentation.budget_ms, Some(60_000));
    }

    #[tokio::test]
    async fn unsupported_actor_without_reader_degrades_refs_before_provider_projection() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(Vec::new()));
        let store: Arc<dyn StoreHandle> = Arc::new(MemoryStore::new());
        let (mut actor, _handle) = HarnessActor::new(actor_config(false), provider, store);
        let mut messages = vec![Message::tool_result_with_images(
            "call-image",
            "captured",
            false,
            (0..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN)
                .map(|index| ImageBlockRef {
                    artifact: ArtifactRef::new(format!("blake3:unresolved-image-{index}")),
                    media_type: "image/png".into(),
                    width: 1,
                    height: 1,
                    byte_len: 1,
                })
                .collect(),
        )];

        let attachments = actor
            .resolve_tool_result_images(&mut messages)
            .await
            .expect("unsupported provider projection");

        assert!(attachments.is_empty());
        let Block::ToolResult {
            preview, images, ..
        } = &messages[0].blocks[0]
        else {
            panic!("expected tool result")
        };
        assert!(images.is_empty());
        assert!(preview.contains("blake3:unresolved-image-0"));
        assert!(preview.contains("oldest first"));
        assert_eq!(
            preview.matches("unavailable to this provider").count(),
            TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN
        );
    }

    #[tokio::test]
    async fn admission_validates_all_refs_without_caching_then_resolution_caches_only_budget() {
        let base = BASE64
            .decode("/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgoKC8oLzc3Nzc3N0E8QUNDQ0FBQUFDQ0NISEhVVVVISEhDQ0hIUFBVVVxfXFdXVVdfX2RkZHh4c3OMjJGsrM//xABMAAEBAAAAAAAAAAAAAAAAAAAABwEBAQAAAAAAAAAAAAAAAAAABQcQAQAAAAAAAAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCAAIABADASIAAhEAAxEA/9oADAMBAAIRAxEAPwCOAL+Kf//Z")
            .expect("valid JPEG fixture");
        let mut objects = HashMap::new();
        let mut images = Vec::new();
        for index in 0_u8..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN as u8 {
            let mut bytes = base[..base.len() - 2].to_vec();
            bytes.extend_from_slice(&[0xff, 0xfe, 0x00, 0x03, index]);
            bytes.extend_from_slice(&[0xff, 0xd9]);
            let artifact = ArtifactRef::new(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
            images.push(ImageBlockRef {
                artifact: artifact.clone(),
                media_type: "image/jpeg".into(),
                width: 16,
                height: 8,
                byte_len: bytes.len() as u64,
            });
            objects.insert(artifact, bytes);
        }
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(Vec::new()));
        let store: Arc<dyn StoreHandle> = Arc::new(MemoryStore::new());
        let reader: Arc<dyn ArtifactReader> = Arc::new(ImageReaderFixture { objects });
        let (mut actor, _handle) = HarnessActor::new_with_dispatcher_and_artifacts(
            actor_config(true),
            provider,
            store,
            None,
            Some(reader),
        );

        actor
            .admit_tool_result_images(&images)
            .await
            .expect("pre-journal image admission");
        assert!(actor.resolved_tool_images.is_empty());
        assert_eq!(actor.validated_tool_image_refs.len(), images.len());

        let mut messages = vec![Message::tool_result_with_images(
            "call-many-images",
            "captured",
            false,
            images.clone(),
        )];
        let attachments = actor
            .resolve_tool_result_images(&mut messages)
            .await
            .expect("budgeted provider resolution");
        assert_eq!(attachments.len(), TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN);
        assert_eq!(
            actor.resolved_tool_images.len(),
            TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN
        );
        let Block::ToolResult {
            preview,
            images: retained,
            ..
        } = &messages[0].blocks[0]
        else {
            panic!("expected tool result")
        };
        assert_eq!(retained, &images[1..]);
        assert!(preview.contains(images[0].artifact.as_str()));
    }
}
