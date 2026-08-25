//! CHARTER — the turn engine: owned per-session supervisors and injectable
//! turn dependencies (report R1).
//!
//! What lives here: [`WorkerManager`] (one lazy supervisor per session, all
//! tasks owned — nothing detached), the supervisor loop (accepted-turn
//! queue, active cancellation, provider/tool/prompt assembly, drain
//! settlement), the turn-scoped [`ProviderFactory`]/[`TurnToolFactory`]
//! ports, and the production broker-backed tool dispatcher with its
//! hub-owned journal/CAS adapters. What may NOT live here: SQLite (a worker
//! holds only its lease-fenced `HubStoreHandle`; a source-scan regression
//! test enforces the module-side half of the R1 append-exclusivity seal),
//! wire/RPC concerns
//! (rpc.rs hands this module a COMMITTED [`AcceptedTurn`], never a raw
//! request), and session-hub actor work (the hub actor must stay free of
//! provider/tool awaits; everything slow happens in supervisor tasks).
//!
//! ADMISSION DISCIPLINE (authoritative statement): a supervisor starts
//! provider work only from durable facts — a submit reaches it after the
//! acceptance transaction committed, and `admit_pending`/`refill_queued_turns`
//! re-derive runnability from journal run states, never from the in-memory
//! message that delivered the hint. The bounded queue may drop hints
//! (`rescan_needed`); the durable `Queued`+`UserMessage` pair is the overflow
//! buffer.

#[cfg(test)]
#[path = "cu1_image_runtime_tests.rs"]
mod cu1_image_runtime_tests;
#[cfg(test)]
#[path = "cu2_computer_runtime_tests.rs"]
mod cu2_computer_runtime_tests;
#[cfg(test)]
#[path = "g1_todo_runtime_tests.rs"]
mod g1_todo_runtime_tests;
#[cfg(test)]
#[path = "image_event_runtime_tests.rs"]
mod image_event_runtime_tests;
#[cfg(test)]
#[path = "mobile_runtime_tests.rs"]
mod mobile_runtime_tests;
#[cfg(test)]
#[path = "pair_switch_runtime_tests.rs"]
mod pair_switch_runtime_tests;
#[cfg(test)]
#[path = "wd_pdf_runtime_tests.rs"]
mod wd_pdf_runtime_tests;

use crate::delegation::{DelegationHandle, MessageCoordinates, SpawnCoordinates};
use crate::diagnostics::{EffectBreadcrumb, EffectDiagnostics};
use crate::image_events::{detect_created_images, image_created_payload};
use crate::project_instructions::{self, LoadedProjectInstructions};
use crate::session_hub::{HubStoreHandle, SessionHub, SessionHubError};
use crate::turn_recovery::{cancelled_resumption_payloads, failed_resumption_payloads};
use async_trait::async_trait;
use base64::Engine;
use haider_core::{
    AcceptedRunRetry, AcceptedShellExec, AcceptedTurn, CancelToken, ChildWaitCheckpoint,
    CompiledPromptProjection, ComputerEvidenceCommand, ComputerEvidenceOutcome,
    ContextCompactionClaim, ContextCompactionReceiptResponse, ContextCompactor, DeferredTicket,
    DeferredToolResult, EventIdGenerator, FinalizationGuard, FinalizationGuardDecision,
    GraphEvidenceCommand, GraphEvidenceOutcome, GraphFinalizationCommand, GraphFinalizationOutcome,
    GraphSwitchCommand, GraphSwitchOutcome, HarnessActor, HarnessConfig, PartialStreamCheckpoint,
    PreviousCacheRequest, ProcessSignalCommand, ProcessSignalOutcome, PromptHistoryCompiler,
    ProviderDerivedRequestState, ProviderPairSwitch, ProviderPairSwitchCommitter,
    RequestInputCheckpoint, SessionSelectModelCommand, SessionSelectModelOutcome, StoreHandle,
    SubmitCheckpointTurn, SubmitChildWaitTurn, SubmitCommittedTurn, SubmitPartialStreamTurn,
    ToolDispatchResult, ToolDispatcher, TurnHandle, build_cache_request_diagnostic,
    classify_cache_request, context_soft_threshold_tokens, effect_recovery_evidence,
    estimate_provider_request_input_tokens, presentation_for_haider_error,
    sanitized_failure_message,
};
use haider_protocol::agent::Grant;
use haider_protocol::cache::{
    CacheEpochTransitionReason, CacheEpochTransitionV1, CacheRequestAttemptV1,
};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase, FileFreshness,
    WorkspaceMutation,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::graph::{
    ComputerObservationKind, GraphNodeName, GraphPhase, ProcessSignalRecorded, ProcessSignalRef,
    WorkspaceMutationRef, process_signal_subject_digest,
};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind, TreeNode,
};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EffectId, EventId, GraphId, ItemId, MenuId, NodeId, RunId,
    SessionId,
};
use haider_protocol::image::{IMAGE_CREATED_EXTENSION_KIND, ImageCreatedV1};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem, UserCommandOriginV1};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope,
    effect_recovery_menu,
};
use haider_protocol::permission::{
    PermissionEventPayload, PermissionGrantAction, PermissionGrantNeeded,
    PermissionGrantResolution, PermissionGrantResolved,
};
use haider_protocol::project_instructions::ProjectInstructionsLoaded;
use haider_protocol::provider::{
    AccountUsage, CacheBoundaryIdentity, CacheControlObservationV1, CacheRewarmReasonV1,
    CacheStatAvailability, CapabilityDoc, FeatureResolve, FinishReason, PrefixDigests,
    RequestUsage, StreamEvent, Usage, UsageRequestKind, UsageScope,
};
use haider_protocol::queue::QueueChange;
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::{
    BoundedResult, DispatchMode, ImageBlockRef, RememberedGrantScope, RememberedSessionGrant,
    ToolInventoryEntry, ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
    ToolResultStatus,
};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, DEEPSEEK_PROVIDER_NAME, Message,
    OPENAI_OAUTH_PROVIDER_NAME, PromptCacheMetadata, ProviderCredentialSurface, ResolvedAttachment,
    apply_tool_result_image_budget, canonical_tool_definitions_digest,
    degrade_tool_result_images_to_placeholders,
};
use haider_provider::{Provider, ToolDefinition, TurnRequest};
use haider_store::{MenuResolutionCommand, MenuResolutionOutcome};
use haider_tools::{
    CasSink, ChangeLedger, CommandOutputSink, ComputerBackend, ComputerCancelToken, ComputerError,
    ComputerOperation, ComputerOutput, ComputerPermissionPoll, EffectBroker, FsCaseMode, FsEdit,
    FsEditChange, FsGlob, FsPath, FsPathOperation, FsRead, FsSearch, FsSearchMode, FsWrite,
    GraphEvidence, JournalSink, MessageSubagent, MobileBackend, MobileCancelToken, MobileError,
    MobileOperation, PermissionPolicy, ProcessBounds, ProcessExec, ProcessResult, ResultBounds,
    ScreenshotRedactionPolicy, SessionGrant, SessionGrantScope, ShellSession, SpawnSubagent,
    ToolError, ToolResult, TurnAttribution, WebFetch, WorkflowAuthor,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const MANAGER_CAPACITY: usize = 128;
const SUPERVISOR_CAPACITY: usize = 64;
const COMPUTER_PERMISSION_POLL_TIMEOUT: Duration = Duration::from_secs(120);
const COMPUTER_PERMISSION_MENU_ORIGIN: &str = "computer-os-permission";

/// A provider resolved and pinned for one logical turn (R6).
pub struct ResolvedTurnProvider {
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model: String,
    /// Exact provider-catalog declaration for `model`; never inferred.
    pub context_window: Option<u64>,
    /// Stamped into usage until an automatic pre-event rotation changes it.
    pub account_alias: Option<String>,
    /// Factory-time alternate committed before the first provider call.
    pub initial_rotation: Option<haider_protocol::credential::RotationEvent>,
    /// The initial resolution already spent the logical turn's one hop.
    pub rotation_budget_consumed: bool,
    /// Daemon-owned live credential resolver for this logical turn.
    pub attempt_resolver: Option<Arc<dyn haider_core::ProviderAttemptResolver>>,
    /// Optional pre-resolved, strictly larger same-provider lane for the
    /// compaction runaway guard.
    pub compaction_promotion: Option<haider_core::ProviderPairSwitchTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedCompaction {
    pub run_id: RunId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
    pub branch_id: Option<BranchId>,
}

/// Injectable, turn-scoped provider resolver (R6/R8): initial resolution
/// happens after durable acceptance and before provider work. Core may replace
/// that result only through the returned resolver, once per logical turn and
/// only before the current request emits an event. Manual login/account
/// changes still affect the next logical turn. `resolve_for_turn` must return
/// the provider name recorded in session metadata; `start_turn` rejects a
/// mismatch.
///
/// F1 extension: the `metadata` argument is itself re-read from the store per
/// logical turn (`fresh_turn_metadata`), so a committed
/// `session.select_model` pair is what the next turn resolves through.
/// Sessions are provider-agnostic — the pair is the CURRENT model selection,
/// not session identity.
#[async_trait]
pub trait ProviderFactory: Send + Sync {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError>;

    /// W-B: pair resolution with the session's web-capability degrades in
    /// hand, so a latched anthropic degrade clears the native web-tools
    /// declaration at CONSTRUCTION. Defaulted so every existing factory —
    /// production and test — keeps its exact behavior; the accounts factory
    /// overrides it.
    async fn resolve_for_turn_with_web(
        &self,
        metadata: &SessionMetadataV1,
        degrade: WebCapabilityDegrade,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let _ = degrade;
        self.resolve_for_turn(metadata).await
    }

    /// Gives production factories a chance to delete provider-owned
    /// ephemeral cache resources when a session switches wire families.
    async fn reconcile_cache_scope(&self, _session_id: &SessionId, _provider: &str) {}
}

/// W-B: session-scoped web-capability degrades (hub-owned, in-memory).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebCapabilityDegrade {
    /// Anthropic server web tools 400ed this session: declare none and fall
    /// back to the local `web_fetch` client tool ("local fallback on
    /// refusal", capability matrix).
    pub anthropic_web_tools: bool,
    /// The codex alpha/search endpoint returned 404/410 this session: stop
    /// advertising the client `web_search` tool (no retry storm).
    pub openai_alpha_search: bool,
}

/// W-B (decision 3): daemon-side executor for the CLIENT `web_search` tool —
/// a POST to the codex alpha/search endpoint under the SAME subscription
/// credential as turns. Injectable so tests never dial the real endpoint.
#[async_trait]
pub(crate) trait WebSearchExecutor: Send + Sync {
    async fn search(
        &self,
        model: &str,
        session_id: &str,
        query: &str,
    ) -> Result<String, WebSearchFailure>;
}

/// Typed failure from one web-search execution. `degraded` marks the
/// endpoint as GONE (404/410) — the session capability latch, not a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSearchFailure {
    pub(crate) message: String,
    pub(crate) degraded: bool,
}

/// Bridges core's automatic mid-turn lane switch to the exact actor-owned,
/// receipted transaction used by the public `session.select_model` RPC.
struct DaemonProviderPairSwitchCommitter {
    store: HubStoreHandle,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
}

impl std::fmt::Debug for DaemonProviderPairSwitchCommitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonProviderPairSwitchCommitter")
            .field("session_id", self.store.session_id())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderPairSwitchCommitter for DaemonProviderPairSwitchCommitter {
    async fn commit(&self, switch: &ProviderPairSwitch) -> Result<(), HaiderError> {
        let request_json = serde_json::json!({
            "automatic": true,
            "session_id": self.store.session_id(),
            "run_id": switch.run_id,
            "switch_ordinal": switch.switch_ordinal,
            "worker_generation": self.store.worker_generation(),
            "from_provider": switch.from_provider,
            "from_model": switch.from_model,
            "provider": switch.to_provider,
            "model": switch.to_model,
            "cause": switch.cause.as_str(),
        })
        .to_string();
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let command_id = format!(
            "automatic-pair-switch-{}",
            request_digest.get(..24).unwrap_or(&request_digest)
        );
        let command = SessionSelectModelCommand {
            command_id,
            request_digest,
            request_json,
            session_id: self.store.session_id().clone(),
            worker_generation: self.store.worker_generation(),
            provider: switch.to_provider.clone(),
            model: switch.to_model.clone(),
            // rev933b finding 7: the automatic switch observed this exact
            // pair; a concurrent explicit selection moves it and must win.
            expected_pair: Some((switch.from_provider.clone(), switch.from_model.clone())),
            event_id: self.event_ids.next(),
            device_id: self.device_id.clone(),
        };
        match self.store.hub().select_session_model(command).await {
            Ok(
                SessionSelectModelOutcome::Committed { .. }
                | SessionSelectModelOutcome::IdempotentReplay { .. },
            ) => Ok(()),
            Err(error) => Err(hub_error(error)),
        }
    }
}

struct DaemonContextCompactor {
    store: HubStoreHandle,
    provider: Arc<dyn Provider>,
    model: String,
    max_tokens: u64,
    context_window: Option<u64>,
    reserved_output_tokens: u64,
    post_compaction_system_prompt: Option<String>,
    post_compaction_tools: Vec<ToolDefinition>,
    reasoning_settings: String,
    cache_expected_later_reads: u32,
    cache_reuse_gap_ms: Option<u64>,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
    agent_id: Option<AgentId>,
    branch_id: Option<BranchId>,
    usage_scope: UsageScope,
    usage_account: Option<haider_protocol::ids::CredentialAlias>,
}

const COMPACTION_SUMMARY_INSTRUCTION: &str = "Summarize the preceding conversation for lossless continuation. Preserve decisions, constraints, exact identifiers, unresolved work, and tool outcomes. Return only the summary.";

impl std::fmt::Debug for DaemonContextCompactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonContextCompactor")
            .field("session_id", self.store.session_id())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl DaemonContextCompactor {
    async fn record_cache_request_attempt(
        &self,
        run_id: &RunId,
        ordinal: u64,
        diagnostic: &haider_protocol::provider::CacheRequestDiagnosticV1,
    ) -> Result<(), HaiderError> {
        let item = CacheRequestAttemptV1 {
            ordinal,
            diagnostic: diagnostic.clone(),
        }
        .extension_item()
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("cache request diagnostic could not serialize: {error}"),
                false,
            )
        })?;
        let item_id = ItemId::new(format!(
            "cache-request-attempt-{}-{ordinal}",
            self.event_ids.next()
        ));
        let mut envelopes = vec![
            supervisor_envelope(
                &self.store,
                &self.device_id,
                self.branch_id.clone(),
                Some(run_id.clone()),
                self.event_ids.next(),
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
            )?,
            supervisor_envelope(
                &self.store,
                &self.device_id,
                self.branch_id.clone(),
                Some(run_id.clone()),
                self.event_ids.next(),
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
            )?,
        ];
        for envelope in &mut envelopes {
            envelope.agent_id = self.agent_id.clone();
            envelope.render.ui = false;
        }
        StoreHandle::append(&self.store, &mut envelopes)
            .await
            .map(|_| ())
    }

    fn compaction_cache_metadata(
        &self,
        messages: &[Message],
        stable_history_end: usize,
        previous_stable_history_end: Option<usize>,
        prefix_digests: PrefixDigests,
        latest_compaction_summary_end: Option<usize>,
    ) -> PromptCacheMetadata {
        // Hygiene round: the LIVE boundary from the caller — a second
        // in-turn compaction marks the FIRST one's summary, which a value
        // frozen at compactor construction could never know about.
        let latest_compaction_summary_end = latest_compaction_summary_end
            .filter(|boundary| *boundary > 0 && *boundary <= stable_history_end);
        let compaction_epoch = latest_compaction_summary_end.map_or_else(
            || digest_json(&"root-compaction-epoch"),
            |boundary| digest_json(&messages[boundary - 1]),
        );
        let cache_epoch = digest_json(&serde_json::json!({
            "provider": self.usage_scope.provider,
            "model": self.model,
            "account_scope": self.usage_account,
            "system_digest": prefix_digests.system,
            "tool_digest": prefix_digests.tools,
            "auth_digest": prefix_digests.auth_mode,
            "reasoning_digest": prefix_digests.reasoning_settings,
            "compaction_epoch": compaction_epoch,
        }));
        let stable_prefix_tokens = estimate_provider_request_input_tokens(
            &messages[..stable_history_end],
            &self.post_compaction_system_prompt,
            &self.post_compaction_tools,
            &[],
        );
        PromptCacheMetadata {
            stable_history_end,
            current_user_start: stable_history_end,
            previous_stable_history_end,
            latest_compaction_summary_end,
            prefix_digests,
            cache_epoch,
            compaction_epoch,
            provider: self.usage_scope.provider.clone(),
            session_scope: self.store.session_id().as_str().to_owned(),
            account_scope: self
                .usage_account
                .as_ref()
                .map(|scope| scope.as_str().to_owned()),
            stable_prefix_tokens,
            expected_later_reads: self.cache_expected_later_reads,
            reuse_gap_ms: self.cache_reuse_gap_ms,
        }
    }
}

struct DaemonGraphFinalizationGuard {
    store: HubStoreHandle,
    branch_id: Option<BranchId>,
    device_id: DeviceId,
}

impl std::fmt::Debug for DaemonGraphFinalizationGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonGraphFinalizationGuard")
            .field("session_id", self.store.session_id())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl FinalizationGuard for DaemonGraphFinalizationGuard {
    async fn before_done(&self, run_id: &RunId) -> Result<FinalizationGuardDecision, HaiderError> {
        let outcome = self
            .store
            .hub()
            .guard_graph_finalization(GraphFinalizationCommand {
                session_id: self.store.session_id().clone(),
                branch_id: self.branch_id.clone(),
                run_id: run_id.clone(),
                worker_generation: self.store.worker_generation(),
                device_id: self.device_id.clone(),
            })
            .await
            .map_err(hub_error)?;
        Ok(match outcome {
            GraphFinalizationOutcome::AllowDone => FinalizationGuardDecision::AllowDone,
            GraphFinalizationOutcome::Deferred {
                graph_id,
                emit_reminder,
                ..
            } => FinalizationGuardDecision::Continue {
                reminder: emit_reminder.then(|| {
                    format!(
                        "The active workflow {graph_id} still has unmet obligations. Continue working and satisfy them, or explicitly abandon the workflow before finalizing."
                    )
                }),
            },
            GraphFinalizationOutcome::ConfirmRequired { menu, .. } => {
                FinalizationGuardDecision::ConfirmRequired(menu)
            }
        })
    }
}

#[async_trait]
impl ContextCompactor for DaemonContextCompactor {
    async fn plan(
        &self,
        run_id: &RunId,
        resume_cause: CompactionResume,
    ) -> Result<CompactionIntent, HaiderError> {
        PromptHistoryCompiler::plan_compaction(
            &self.store,
            self.store.session_id(),
            self.branch_id.as_ref(),
            self.agent_id.as_ref(),
            run_id,
            format!("compact-{}", self.event_ids.next()),
            resume_cause,
        )
        .await
    }

    async fn compact(
        &self,
        run_id: &RunId,
        intent: &CompactionIntent,
        covered_messages: Vec<Message>,
        attachments: Vec<haider_provider::ResolvedAttachment>,
        latest_compaction_summary_end: Option<usize>,
    ) -> Result<Message, HaiderError> {
        let (previous_cache_request, _) =
            prior_cache_request_context(&self.store, &self.usage_scope).await?;
        let previous_stable_history_end = previous_cache_request
            .as_ref()
            .map(|previous| previous.history_message_count);
        let immutable_history_digest = digest_json(&covered_messages);
        let covered_history_end = covered_messages.len();
        let mut replay_messages = covered_messages.clone();
        replay_messages.push(Message::user_text(COMPACTION_SUMMARY_INSTRUCTION));
        let mut prefix_digests = PrefixDigests {
            system: digest_json(&self.post_compaction_system_prompt),
            tools: canonical_tool_definitions_digest(&self.post_compaction_tools),
            immutable_history: immutable_history_digest.clone(),
            model: digest_json(&self.model),
            auth_mode: digest_json(&self.usage_scope.auth_scope),
            reasoning_settings: digest_json(&self.reasoning_settings),
        };
        let mut previous_prefix_digests = previous_stable_history_end
            .filter(|previous| *previous <= replay_messages.len())
            .map(|previous| PrefixDigests {
                system: prefix_digests.system.clone(),
                tools: prefix_digests.tools.clone(),
                immutable_history: digest_json(&replay_messages[..previous]),
                model: prefix_digests.model.clone(),
                auth_mode: prefix_digests.auth_mode.clone(),
                reasoning_settings: prefix_digests.reasoning_settings.clone(),
            });
        let mut cache_metadata = self.compaction_cache_metadata(
            &replay_messages,
            covered_history_end,
            previous_stable_history_end,
            prefix_digests.clone(),
            latest_compaction_summary_end,
        );
        let mut request = TurnRequest {
            messages: replay_messages,
            model: self.model.clone(),
            // Round 5: a lossless summary of a near-window history needs
            // real output room — 4K forced truncation into total failure.
            max_tokens: self.max_tokens.min(8_192),
            system_prompt: self.post_compaction_system_prompt.clone(),
            tools: self.post_compaction_tools.clone(),
            // Round 5: the ACTOR resolved these exactly as the live lane
            // does, so an image-bearing prefix replays instead of always
            // detouring through the uncached fallback.
            attachments,
            cache_metadata: Some(cache_metadata.clone()),
        };
        let prepared = self.provider.prepare_turn(&request);
        if let Some(rendered) = prepared.as_ref().map(|prepared| prepared.prefix_digests()) {
            prefix_digests = rendered.clone();
            previous_prefix_digests = prepared
                .as_ref()
                .and_then(|prepared| prepared.previous_immutable_history_digest())
                .map(|history| {
                    let mut previous = rendered.clone();
                    previous.immutable_history = history.to_owned();
                    previous
                });
            cache_metadata = self.compaction_cache_metadata(
                &request.messages,
                covered_history_end,
                previous_stable_history_end,
                prefix_digests.clone(),
                latest_compaction_summary_end,
            );
            request.cache_metadata = Some(cache_metadata.clone());
        }
        let cache_control = prepared
            .as_ref()
            .map_or(CacheControlObservationV1::Unavailable, |prepared| {
                *prepared.cache_control()
            });
        let replay_cache_diagnostic = build_cache_request_diagnostic(
            &self.store.hub().cache_diagnostic_key(),
            &self.usage_scope.provider,
            &self.model,
            &cache_metadata.cache_epoch,
            &prefix_digests,
            previous_prefix_digests.as_ref(),
            previous_cache_request.as_ref(),
            covered_history_end,
            cache_metadata.stable_prefix_tokens,
            cache_metadata.reuse_gap_ms,
            cache_control,
            None,
        );
        self.record_cache_request_attempt(run_id, 1, &replay_cache_diagnostic)
            .await?;
        let replay_request_messages = request.messages.clone();
        let (
            mut stream,
            request_messages,
            request_ordinal,
            request_cache_epoch,
            request_cache_diagnostic,
        ) = match self.provider.stream_prepared_turn(request, prepared).await {
            Ok(stream) => (
                stream,
                replay_request_messages,
                1_u64,
                cache_metadata.cache_epoch.clone(),
                replay_cache_diagnostic,
            ),
            // Round 5: a RETRYABLE start failure (transport, overload,
            // rate limit) propagates so the caller's retry semantics run —
            // burning it as an uncached full-price fallback both lies about
            // the failure class and pays for the lie.
            Err(error) if error.retryable => {
                return Err(HaiderError::new(
                    ErrorCode::ProviderError,
                    format!("context summarization could not start: {error}"),
                    true,
                ));
            }
            Err(_) => {
                // Some provider families cannot replay durable multimodal
                // blocks. Preserve the old text-only request as a single
                // degraded fallback, but never mutate the cache-riding
                // attempt or the history digest describing this summary.
                let mut degraded_messages = covered_messages.clone();
                for message in &mut degraded_messages {
                    message.blocks.retain(|block| {
                        !matches!(
                            block,
                            haider_protocol::provider::Block::Attachment(
                                haider_protocol::tool::AttachmentBlock::Image { .. }
                            )
                        )
                    });
                }
                let artifact_store = self.store.clone();
                prepare_tool_images_for_text_only_request(&artifact_store, &mut degraded_messages)
                    .await?;
                degraded_messages.push(Message::user_text(COMPACTION_SUMMARY_INSTRUCTION));
                let request_messages = degraded_messages.clone();
                let fallback = TurnRequest {
                    messages: degraded_messages,
                    model: self.model.clone(),
                    // Round 6: the degraded path needs the SAME output room
                    // as the replay — truncation is truncation either way.
                    max_tokens: self.max_tokens.min(8_192),
                    system_prompt: None,
                    tools: Vec::new(),
                    attachments: Vec::new(),
                    cache_metadata: None,
                };
                let fallback_prefix_digests = PrefixDigests {
                    system: digest_json(&Option::<String>::None),
                    tools: canonical_tool_definitions_digest(&[]),
                    immutable_history: immutable_history_digest.clone(),
                    model: digest_json(&self.model),
                    auth_mode: digest_json(&self.usage_scope.auth_scope),
                    // Round 5: the same configured provider ran this request
                    // — usage must not claim a default it did not use.
                    reasoning_settings: digest_json(&self.reasoning_settings),
                };
                let fallback_stable_prefix_tokens = estimate_provider_request_input_tokens(
                    &request_messages[..covered_history_end.min(request_messages.len())],
                    &None,
                    &[],
                    &[],
                );
                let fallback_cache_epoch = digest_json(&serde_json::json!({
                    "provider": self.usage_scope.provider,
                    "model": self.model,
                    "account_scope": self.usage_account,
                    "system_digest": fallback_prefix_digests.system,
                    "tool_digest": fallback_prefix_digests.tools,
                    "auth_digest": fallback_prefix_digests.auth_mode,
                    "reasoning_digest": fallback_prefix_digests.reasoning_settings,
                    "compaction_epoch": cache_metadata.compaction_epoch,
                }));
                let fallback_cache_diagnostic = build_cache_request_diagnostic(
                    &self.store.hub().cache_diagnostic_key(),
                    &self.usage_scope.provider,
                    &self.model,
                    &fallback_cache_epoch,
                    &fallback_prefix_digests,
                    None,
                    previous_cache_request.as_ref(),
                    covered_history_end,
                    fallback_stable_prefix_tokens,
                    cache_metadata.reuse_gap_ms,
                    CacheControlObservationV1::Unavailable,
                    None,
                );
                self.record_cache_request_attempt(run_id, 2, &fallback_cache_diagnostic)
                    .await?;
                let stream = self.provider.stream_turn(fallback).await.map_err(|error| {
                    HaiderError::new(
                        ErrorCode::ProviderError,
                        format!("context summarization could not start: {error}"),
                        error.retryable,
                    )
                })?;
                (
                    stream,
                    request_messages,
                    2_u64,
                    fallback_cache_epoch,
                    fallback_cache_diagnostic,
                )
            }
        };
        let mut summary = String::new();
        let mut finished = false;
        let mut reported_usage: Option<Usage> = None;
        while let Some(item) = stream.recv().await {
            match item.map_err(|error| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    format!("context summarization failed: {error}"),
                    error.retryable,
                )
            })? {
                StreamEvent::TextDelta { text } => summary.push_str(&text),
                StreamEvent::ReasoningDelta { .. } => {}
                StreamEvent::UsageUpdate(mut usage) => {
                    let mut scope = self.usage_scope.clone();
                    scope.request_kind = UsageRequestKind::Compaction;
                    scope.run = Some(run_id.clone());
                    scope.agent = self.agent_id.clone();
                    scope.cache_epoch.clone_from(&request_cache_epoch);
                    scope.prefix_digests = None;
                    if let Some(account) = &self.usage_account {
                        usage.account = Some(account.clone());
                        scope.account_scope = Some(account.clone());
                    }
                    usage.scope = Some(scope.clone());
                    usage.cache_cost = usage.normalized.as_ref().and_then(|normalized| {
                        haider_provider::estimate_cache_input_costs(&self.model, normalized)
                    });
                    if let Some(account) = &self.usage_account {
                        usage.accounts = vec![AccountUsage {
                            account: account.clone(),
                            input: usage.input,
                            output: usage.output,
                            reasoning: usage.reasoning,
                            cached: usage.cached,
                            source: usage.source,
                            normalized: usage.normalized.clone(),
                            scope: Some(scope),
                            cache_cost: usage.cache_cost,
                        }];
                    }
                    let mut cache_diagnostic = request_cache_diagnostic.clone();
                    cache_diagnostic.classification =
                        classify_cache_request(&cache_diagnostic, usage.normalized.as_ref());
                    usage.request = Some(RequestUsage {
                        ordinal: request_ordinal,
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
                        cache: Some(cache_diagnostic),
                    });
                    // Provider streaming usage is cumulative within this
                    // request; the latest snapshot replaces earlier ones.
                    reported_usage = Some(usage);
                }
                StreamEvent::Finish {
                    reason: FinishReason::EndTurn,
                } => {
                    finished = true;
                    break;
                }
                StreamEvent::Finish { reason } => {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        format!("context summarization ended with {reason:?}"),
                        false,
                    ));
                }
                // Provider bookkeeping, not structure: the codex
                // responses-lite stream emits opaque reasoning fragments on
                // EVERY turn — rejecting them made live summarization fail
                // 100% on openai-oauth (probe autopsy, v0.0.42 battery).
                // W-B: server-executed web tool activity and cited sources
                // are equally incidental to a summarization request.
                StreamEvent::ProviderOpaque { .. }
                | StreamEvent::ServerToolUse { .. }
                | StreamEvent::ServerToolResult { .. }
                | StreamEvent::WebSources { .. } => {}
                StreamEvent::RefusalDelta { .. } => {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        "context summarization was refused by the provider",
                        false,
                    ));
                }
                StreamEvent::ToolCallStart { .. }
                | StreamEvent::ToolCallArgsDelta { .. }
                | StreamEvent::ToolCallEnd { .. } => {
                    return Err(HaiderError::new(
                        ErrorCode::ProviderError,
                        "context summarization returned tool calls",
                        false,
                    ));
                }
            }
        }
        if !finished || summary.trim().is_empty() {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "context summarization returned no completed summary",
                false,
            ));
        }

        let artifact = self.store.put_artifact(summary.as_bytes().to_vec()).await?;
        // Snapshot the journal head before deriving the tree parent. If any
        // turn, steer, or other append races this scan, the actor-side CAS
        // below rejects the compaction node instead of forking off a stale
        // parent and orphaning the concurrently accepted history.
        let expected_head = StoreHandle::latest_seq(&self.store, self.store.session_id()).await?;
        let parent = PromptHistoryCompiler::latest_head(
            &self.store,
            self.store.session_id(),
            self.branch_id.as_ref(),
            self.agent_id.as_ref(),
        )
        .await?;
        let tokens_before = approximate_message_tokens(&request_messages);
        let tokens_after = approximate_text_tokens(&summary);
        let item_id = ItemId::new(format!("compaction-item-{}", intent.operation_id));
        let item = TurnItem::ContextCompaction {
            summary_artifact: artifact.clone(),
            tokens_before: Some(tokens_before),
            tokens_after: Some(tokens_after),
        };
        let node = TreeNode {
            node: NodeId::new(format!("compaction-node-{}", intent.operation_id)),
            parent,
            kind: NodeKind::Compaction {
                covers_from: intent.covers_from.clone(),
                covers_to: intent.covers_to.clone(),
                summary_artifact: artifact,
                tokens_before,
                tokens_after,
                resume_cause: intent.resume_cause.clone(),
            },
        };
        let mut payloads = vec![
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: item.clone(),
            }),
            EventPayload::Item(ItemEvent::Completed { item_id, item }),
            EventPayload::NodeCommitted(node),
        ];
        if let Some(usage) = reported_usage {
            payloads.push(EventPayload::Usage(usage));
        }
        if intent.resume_cause == CompactionResume::ManualIdle {
            let post_compaction_messages = [Message::user_text(summary.clone())];
            let post_compaction_input = estimate_provider_request_input_tokens(
                &post_compaction_messages,
                &self.post_compaction_system_prompt,
                &self.post_compaction_tools,
                &[],
            );
            let footprint = ContextFootprint {
                input_tokens: post_compaction_input,
                output_tokens: 0,
                cached_input_tokens: 0,
                used_tokens: post_compaction_input,
                context_window: self.context_window,
                reserved_output_tokens: self.reserved_output_tokens,
                soft_threshold_tokens: self.context_window.and_then(|window| {
                    context_soft_threshold_tokens(window, self.reserved_output_tokens)
                }),
                estimated_turns_to_threshold: None,
                truth: ContextFootprintTruth::Estimated,
            };
            let footprint_item = footprint.extension_item().map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("manual compaction footprint could not serialize: {error}"),
                    false,
                )
            })?;
            let footprint_item_id =
                ItemId::new(format!("compaction-footprint-{}", intent.operation_id));
            payloads.extend([
                EventPayload::Item(ItemEvent::Started {
                    item_id: footprint_item_id.clone(),
                    item: footprint_item.clone(),
                }),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: footprint_item_id,
                    item: footprint_item,
                }),
            ]);
        }
        payloads.push(EventPayload::RunState(match intent.resume_cause {
            CompactionResume::AutoMidTurn => RunState::Thinking,
            CompactionResume::ManualIdle => RunState::Done,
        }));
        let mut envelopes = payloads
            .into_iter()
            .map(|payload| {
                let mut envelope = supervisor_envelope(
                    &self.store,
                    &self.device_id,
                    self.branch_id.clone(),
                    Some(run_id.clone()),
                    self.event_ids.next(),
                    payload,
                )?;
                envelope.agent_id = self.agent_id.clone();
                Ok(envelope)
            })
            .collect::<Result<Vec<_>, HaiderError>>()?;
        self.store
            .append_at_head(expected_head, &mut envelopes)
            .await?;
        Ok(Message::user_text(summary))
    }
}

fn approximate_message_tokens(messages: &[Message]) -> u64 {
    serde_json::to_vec(messages)
        .map(|bytes| approximate_len_tokens(bytes.len()))
        .unwrap_or(u64::MAX)
}

fn approximate_text_tokens(text: &str) -> u64 {
    approximate_len_tokens(text.len())
}

fn approximate_len_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX).saturating_add(3) / 4
}

/// Inputs available to a turn-scoped tool dispatcher factory.
#[derive(Clone)]
pub struct WorkerToolContext {
    pub metadata: SessionMetadataV1,
    pub store: HubStoreHandle,
    pub run_id: RunId,
    pub branch_id: Option<BranchId>,
    pub device_id: DeviceId,
    pub event_ids: Arc<EventIdGenerator>,
    pub(crate) delegation: DelegationHandle,
    pub(crate) tasks: crate::tasks::TaskFacade,
    pub agent_id: Option<AgentId>,
    /// Durable child capability ceiling; root sessions have no ceiling here.
    pub grant: Option<Grant>,
    /// One turn-start snapshot shared by provider advertisement and runtime
    /// dispatch so a concurrent activation cannot split their authority.
    pub(crate) mobile_use_active: bool,
    /// B3 — the typed child's declared-CLI exec scope. `None` = unfenced
    /// (untyped); `Some(vec![])` = deny-all (typed record unresolvable).
    pub cli_scope: Option<Vec<String>>,
    /// W-B: the client web_search executor for this turn (None = typed
    /// unavailable result).
    pub(crate) web_search: Option<Arc<dyn WebSearchExecutor>>,
    pub(crate) diagnostics: Option<Arc<EffectDiagnostics>>,
}

/// Injectable tool/effect boundary (R4). Production uses the shipped broker;
/// tests can hold a dispatch at an exact crash boundary.
///
/// Contract: `definitions` and `create` must agree — every advertised
/// definition must be executable by the created dispatcher (R4 forbids
/// advertising tools a dispatcher cannot run, which can trap a real model in
/// an unproductive loop). `create` returning `None` means the turn runs
/// without general tools and must then advertise none beyond the
/// actor-owned `request_input`.
#[async_trait]
pub trait TurnToolFactory: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError>;
}

/// How the daemon obtains its per-turn provider factory, and — the D3-5
/// whitelist unification — the ONE authority on which providers
/// `session.create` may accept. The rpc.rs hardcoded list is dead; `"fake"`
/// is creatable only when an injected test configuration says so, never on
/// the production wire path.
#[derive(Clone)]
pub enum ProviderFactoryConfig {
    /// Production: resolve each logical turn from the daemon-owned account
    /// store + vault (`crate::accounts::AccountsProviderFactory`), giving
    /// `/login` next-turn pickup with zero worker changes.
    Accounts,
    /// Accounts-backed resolution with an injected provider constructor —
    /// the login→next-turn test seam.
    AccountsWith(Arc<dyn crate::accounts::AccountProviderBuilder>),
    /// Fully injected factory plus its creatable-provider set.
    Injected {
        factory: Arc<dyn ProviderFactory>,
        providers: BTreeSet<String>,
    },
}

impl ProviderFactoryConfig {
    /// Injected test factory whose creatable set is `{"fake"}`.
    pub fn injected(factory: Arc<dyn ProviderFactory>) -> Self {
        Self::Injected {
            factory,
            providers: BTreeSet::from(["fake".to_owned()]),
        }
    }

    /// The providers `session.create` may accept under this configuration.
    pub fn creatable_providers(&self) -> BTreeSet<String> {
        match self {
            Self::Accounts => haider_provider::BUILTIN_PROVIDER_NAMES
                .into_iter()
                .map(str::to_owned)
                .collect(),
            Self::AccountsWith(builder) => builder.providers(),
            Self::Injected { providers, .. } => providers.clone(),
        }
    }
}

/// Runtime dependency bundle. A test replaces factories without changing the
/// production connection, hub, worker, or core execution path.
#[derive(Clone)]
pub struct DaemonDependencies {
    pub provider_factory: ProviderFactoryConfig,
    pub tool_factory: Arc<dyn TurnToolFactory>,
    /// Account machinery (vault, credential validator, descriptor store) —
    /// the W3c2 login seam (`crate::accounts`).
    pub accounts: crate::accounts::AccountsDependencies,
}

impl Default for DaemonDependencies {
    fn default() -> Self {
        Self {
            provider_factory: ProviderFactoryConfig::Accounts,
            tool_factory: Arc::new(BrokerToolFactory),
            accounts: crate::accounts::AccountsDependencies::default(),
        }
    }
}

/// The RESOLVED per-worker dependency bundle `run_inner` hands the manager
/// (the factory selection above collapses to one concrete factory before
/// any worker exists).
#[derive(Clone)]
pub(crate) struct WorkerDependencies {
    pub(crate) provider_factory: Arc<dyn ProviderFactory>,
    pub(crate) tool_factory: Arc<dyn TurnToolFactory>,
    pub(crate) delegation: Option<DelegationHandle>,
    /// W-B: the client `web_search` executor. `None` (test worlds without
    /// one) makes the tool answer with a typed "unavailable" result.
    pub(crate) web_search: Option<Arc<dyn WebSearchExecutor>>,
    pub(crate) diagnostics: Option<Arc<EffectDiagnostics>>,
}

impl WorkerDependencies {
    /// A resolved bundle with NO credential source — the in-crate test
    /// stand-in for a daemon whose account store has nothing to offer.
    #[cfg(test)]
    pub(crate) fn unconfigured_for_tests() -> Self {
        Self {
            provider_factory: Arc::new(UnconfiguredProviderFactory),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
            diagnostics: None,
        }
    }
}

/// Always fails with `CredentialMissing` (test-only resolution stand-in).
#[cfg(test)]
struct UnconfiguredProviderFactory;

#[cfg(test)]
#[async_trait]
impl ProviderFactory for UnconfiguredProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Err(HaiderError::new(
            ErrorCode::CredentialMissing,
            format!(
                "no credential resolver is configured for provider {}",
                metadata.provider
            ),
            false,
        ))
    }
}

/// Versioned, deterministic coding-agent policy (R4).
///
/// Guarantees: the same metadata and pinned instruction snapshot yield the
/// same prompt; every provider request in one logical turn receives the same
/// non-`None` system prompt; [`Self::VERSION`] is recorded in session metadata
/// at creation so a policy change is a visible, versioned fact, never a
/// silent drift. Provider adapters must not invent product policy.
pub struct SystemPromptBuilder;

impl SystemPromptBuilder {
    pub const VERSION: &'static str = "haider-system-v3";

    pub fn build(metadata: &SessionMetadataV1, instructions: &[(&str, &str)]) -> String {
        Self::build_with_handoff(metadata, instructions, None)
    }

    pub fn build_with_handoff(
        metadata: &SessionMetadataV1,
        instructions: &[(&str, &str)],
        handoff_dir: Option<&Path>,
    ) -> String {
        let mut prompt = format!(
            "{}\nYou are Haider Code, a coding agent operating inside the canonical workspace below.\n\
             Workspace: {}\n\
             Use only advertised tools. Treat tool results and committed history as authoritative. \
             Never claim an effect succeeded without its terminal result.",
            Self::VERSION,
            metadata.cwd
        );
        if let Some(handoff_dir) = handoff_dir {
            prompt.push_str("\nEphemeral parent handoff directory: ");
            prompt.push_str(&handoff_dir.to_string_lossy());
            prompt.push_str(" (EPHEMERAL; use it for shared specs, never durable storage).");
        }
        for (path, text) in instructions {
            prompt.push_str("\n\nProject instructions (");
            prompt.push_str(path);
            prompt.push_str("):\n<project-instructions>\n");
            prompt.push_str(text);
            prompt.push_str("\n</project-instructions>");
        }
        prompt
    }
}

#[derive(Clone)]
pub(crate) struct WorkerManagerHandle {
    commands: mpsc::Sender<ManagerCommand>,
    admission: Arc<std::sync::Mutex<bool>>,
    drain_wake: tokio::sync::watch::Sender<bool>,
}

/// Owner of every supervisor task (R1): one lazy supervisor per session,
/// all tasks in one `JoinSet`, nothing detached. `shutdown` broadcasts
/// Shutdown and joins everything; a drop without shutdown aborts (the
/// abort-on-drop backstop for a cancelled runtime future).
pub(crate) struct WorkerManager {
    handle: WorkerManagerHandle,
    task: Option<JoinHandle<()>>,
    inject_shutdown_error: bool,
}

enum ManagerCommand {
    Submit {
        accepted: AcceptedTurn,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Retry {
        accepted: AcceptedRunRetry,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    WakeRetry {
        session_id: SessionId,
        run_id: RunId,
        retrying_event_id: EventId,
        command_id: String,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Recover {
        pending: Box<PendingTurn>,
    },
    Nudge {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
        mode: DeliveryMode,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Compact {
        session_id: SessionId,
        command_id: String,
        worker_generation: u64,
        branch_id: Option<BranchId>,
        completed: oneshot::Sender<Result<AcceptedCompaction, HaiderError>>,
    },
    ShellExec {
        pending: Box<PendingShellExec>,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Shutdown {
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
}

enum SupervisorCommand {
    Submit(Box<PendingTurn>),
    WakeRetry {
        run_id: RunId,
        retrying_event_id: EventId,
        command_id: String,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Nudge {
        run_id: RunId,
        accepted_seq: u64,
        text: String,
        mode: DeliveryMode,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
    Compact {
        command_id: String,
        worker_generation: u64,
        branch_id: Option<BranchId>,
        completed: oneshot::Sender<Result<AcceptedCompaction, HaiderError>>,
    },
    ShellExec(Box<PendingShellExec>),
    Shutdown,
}

pub(crate) struct PendingShellExec {
    pub(crate) accepted: AcceptedShellExec,
    pub(crate) command_id: String,
    pub(crate) branch_id: Option<BranchId>,
    pub(crate) agent_id: Option<AgentId>,
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DurableUserCommandScope {
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
}

/// Owns a shell handoff that raced a durably-terminal provider turn's
/// in-memory cleanup. Same-receipt retries need no second queue slot.
pub(crate) fn defer_shell_handoff(
    deferred: &mut VecDeque<PendingShellExec>,
    pending: PendingShellExec,
) {
    if !deferred
        .iter()
        .any(|queued| queued.accepted.run_id == pending.accepted.run_id)
    {
        deferred.push_back(pending);
    }
}

struct PendingTurn {
    accepted: AcceptedTurn,
    /// Manual retries compile the durable source run's ancestry while the
    /// harness commits all new lifecycle facts under `accepted.run_id`.
    prompt_run_id: Option<RunId>,
    checkpoint: Option<RequestInputCheckpoint>,
    partial_stream: Option<PartialStreamCheckpoint>,
    child_wait: Option<ChildWaitCheckpoint>,
    committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    recovery_ready: Option<oneshot::Sender<Result<(), HaiderError>>>,
    /// Recovery semantics outlive the pre-Ready acknowledgement. In
    /// particular, a queued recovery may be acknowledged behind a parked
    /// checkpoint but must still use recovery-shaped closure if its eventual
    /// provider/credential resolution fails.
    recovering: bool,
}

struct SupervisorSlot {
    sender: mpsc::Sender<SupervisorCommand>,
    task_id: tokio::task::Id,
}

struct SupervisorExit {
    session_id: SessionId,
    terminalize_nonterminal: bool,
}

impl PendingTurn {
    fn accepted(accepted: AcceptedTurn) -> Self {
        Self {
            accepted,
            prompt_run_id: None,
            checkpoint: None,
            partial_stream: None,
            child_wait: None,
            committed_answer: None,
            recovery_ready: None,
            recovering: false,
        }
    }

    fn retry(accepted: AcceptedRunRetry) -> Self {
        Self {
            accepted: AcceptedTurn {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id: None,
                disposition: haider_core::TurnAdmissionDisposition::Started,
                first_user_turn: false,
                pdf_attachments: Vec::new(),
            },
            prompt_run_id: Some(accepted.prompt_run_id),
            checkpoint: None,
            partial_stream: None,
            child_wait: None,
            committed_answer: None,
            recovery_ready: None,
            recovering: false,
        }
    }
}

impl WorkerManager {
    pub(crate) fn start(
        hub: SessionHub,
        mut dependencies: WorkerDependencies,
        inject_shutdown_error: bool,
    ) -> Self {
        if dependencies.delegation.is_none() {
            dependencies.delegation = Some(DelegationHandle::new(hub.clone()));
        }
        let (commands, receiver) = mpsc::channel(MANAGER_CAPACITY);
        let (drain_wake, drain_wakes) = tokio::sync::watch::channel(false);
        let handle = WorkerManagerHandle {
            commands,
            admission: Arc::new(std::sync::Mutex::new(true)),
            drain_wake,
        };
        let task = tokio::spawn(run_manager(hub, dependencies, receiver, drain_wakes));
        Self {
            handle,
            task: Some(task),
            inject_shutdown_error,
        }
    }

    pub(crate) fn handle(&self) -> WorkerManagerHandle {
        self.handle.clone()
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), HaiderError> {
        self.handle.begin_draining();
        if self.inject_shutdown_error {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "injected worker manager shutdown failure",
                true,
            ));
        }
        let (completed, response) = oneshot::channel();
        self.handle
            .commands
            .send(ManagerCommand::Shutdown { completed })
            .await
            .map_err(|_| manager_stopped())?;
        let result = response.await.map_err(|_| manager_stopped())?;
        if let Some(task) = self.task.take() {
            task.await.map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("worker manager shutdown failed: {error}"),
                    true,
                )
            })?;
        }
        result
    }

    /// Abrupt owner teardown for the in-process process-death seam.
    ///
    /// Unlike `shutdown`, this sends no cancellation command and appends no
    /// terminal event. Startup recovery must decide what the durable prefix
    /// means. This is intentionally distinct from a child-supervisor panic:
    /// the live manager observes those through its JoinSet, terminalizes the
    /// run, evicts the slot, and retains/increments the session incarnation
    /// before recreation. Eviction and incarnation are inseparable because a
    /// same-generation EventIdGenerator namespace must never be reused.
    pub(crate) async fn crash(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for WorkerManager {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl WorkerManagerHandle {
    pub(crate) fn begin_draining(&self) {
        if let Ok(mut open) = self.admission.lock() {
            *open = false;
        }
        self.drain_wake.send_replace(true);
    }

    pub(crate) async fn submit(&self, accepted: AcceptedTurn) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        {
            let open = self.admission.lock().map_err(|_| manager_stopped())?;
            if !*open {
                return Err(manager_busy("worker admission is draining"));
            }
            self.commands
                .try_send(ManagerCommand::Submit {
                    accepted,
                    completed,
                })
                .map_err(manager_try_send)?;
        }
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn retry(&self, accepted: AcceptedRunRetry) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        {
            let open = self.admission.lock().map_err(|_| manager_stopped())?;
            if !*open {
                return Err(manager_busy("worker admission is draining"));
            }
            self.commands
                .try_send(ManagerCommand::Retry {
                    accepted,
                    completed,
                })
                .map_err(manager_try_send)?;
        }
        response.await.map_err(|_| manager_stopped())?
    }

    /// Delivers a receipt-backed wake to the exact durable provider backoff.
    /// A missing/already-finished active turn is success: the attempt won the
    /// race naturally, so replay is an idempotent response-only no-op.
    pub(crate) async fn wake_retry(
        &self,
        accepted: &AcceptedRunRetry,
        command_id: String,
    ) -> Result<(), HaiderError> {
        let retrying_event_id = accepted.backoff_event_id.clone().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "backoff-wake receipt has no retrying event id",
                false,
            )
        })?;
        let (completed, response) = oneshot::channel();
        {
            let open = self.admission.lock().map_err(|_| manager_stopped())?;
            if !*open {
                return Err(manager_busy("worker admission is draining"));
            }
            self.commands
                .try_send(ManagerCommand::WakeRetry {
                    session_id: accepted.session_id.clone(),
                    run_id: accepted.run_id.clone(),
                    retrying_event_id,
                    command_id,
                    completed,
                })
                .map_err(manager_try_send)?;
        }
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn nudge(
        &self,
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
    ) -> Result<(), HaiderError> {
        self.deliver_mid_turn(session_id, run_id, accepted_seq, text, DeliveryMode::Steer)
            .await
    }

    pub(crate) async fn subturn(
        &self,
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
    ) -> Result<(), HaiderError> {
        self.deliver_mid_turn(
            session_id,
            run_id,
            accepted_seq,
            text,
            DeliveryMode::Subturn,
        )
        .await
    }

    async fn deliver_mid_turn(
        &self,
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
        mode: DeliveryMode,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .try_send(ManagerCommand::Nudge {
                session_id,
                run_id,
                accepted_seq,
                text,
                mode,
                completed,
            })
            .map_err(manager_try_send)?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn compact(
        &self,
        session_id: SessionId,
        command_id: String,
        worker_generation: u64,
        branch_id: Option<BranchId>,
    ) -> Result<AcceptedCompaction, HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .try_send(ManagerCommand::Compact {
                session_id,
                command_id,
                worker_generation,
                branch_id,
                completed,
            })
            .map_err(manager_try_send)?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn shell_exec(
        &self,
        accepted: AcceptedShellExec,
        command_id: String,
        branch_id: Option<BranchId>,
        agent_id: Option<AgentId>,
        command: String,
        cwd: Option<String>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .try_send(ManagerCommand::ShellExec {
                pending: Box::new(PendingShellExec {
                    accepted,
                    command_id,
                    branch_id,
                    agent_id,
                    command,
                    cwd,
                }),
                completed,
            })
            .map_err(manager_try_send)?;
        response.await.map_err(|_| manager_stopped())?
    }

    fn send_recovery(&self, pending: PendingTurn) -> Result<(), HaiderError> {
        self.commands
            .try_send(ManagerCommand::Recover {
                pending: Box::new(pending),
            })
            .map_err(manager_try_send)
    }

    pub(crate) async fn recover_checkpoint(
        &self,
        accepted: AcceptedTurn,
        checkpoint: RequestInputCheckpoint,
        committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            prompt_run_id: None,
            checkpoint: Some(checkpoint),
            partial_stream: None,
            child_wait: None,
            committed_answer,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_partial_stream(
        &self,
        accepted: AcceptedTurn,
        partial_stream: PartialStreamCheckpoint,
        committed_answer: Option<haider_protocol::envelope::RawEnvelope>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            prompt_run_id: None,
            checkpoint: None,
            partial_stream: Some(partial_stream),
            child_wait: None,
            committed_answer,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_queued(&self, accepted: AcceptedTurn) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            prompt_run_id: None,
            checkpoint: None,
            partial_stream: None,
            child_wait: None,
            committed_answer: None,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_retry(
        &self,
        accepted: AcceptedRunRetry,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        let mut pending = PendingTurn::retry(accepted);
        pending.recovery_ready = Some(completed);
        pending.recovering = true;
        self.send_recovery(pending)?;
        response.await.map_err(|_| manager_stopped())?
    }

    pub(crate) async fn recover_child_wait(
        &self,
        accepted: AcceptedTurn,
        child_wait: ChildWaitCheckpoint,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.send_recovery(PendingTurn {
            accepted,
            prompt_run_id: None,
            checkpoint: None,
            partial_stream: None,
            child_wait: Some(child_wait),
            committed_answer: None,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
        response.await.map_err(|_| manager_stopped())?
    }
}

async fn run_manager(
    hub: SessionHub,
    dependencies: WorkerDependencies,
    mut commands: mpsc::Receiver<ManagerCommand>,
    drain_wakes: tokio::sync::watch::Receiver<bool>,
) {
    let mut supervisors = HashMap::<SessionId, SupervisorSlot>::new();
    let mut incarnations = HashMap::<SessionId, u64>::new();
    let mut task_sessions = HashMap::<tokio::task::Id, SessionId>::new();
    let mut tasks = JoinSet::<SupervisorExit>::new();
    loop {
        let command = tokio::select! {
            biased;
            outcome = tasks.join_next_with_id(), if !tasks.is_empty() => {
                if let Some(outcome) = outcome {
                    handle_supervisor_exit(
                        &hub,
                        &mut supervisors,
                        &mut task_sessions,
                        &mut incarnations,
                        outcome,
                    ).await;
                }
                continue;
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            break;
        };
        match command {
            ManagerCommand::Submit {
                accepted,
                completed,
            } => {
                let result = match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    accepted.session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor
                        .try_send(SupervisorCommand::Submit(Box::new(PendingTurn::accepted(
                            accepted,
                        ))))
                        .map_err(supervisor_try_send),
                    Err(error) => Err(error),
                };
                let _ = completed.send(result);
            }
            ManagerCommand::Retry {
                accepted,
                completed,
            } => {
                let result = match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    accepted.session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor
                        .try_send(SupervisorCommand::Submit(Box::new(PendingTurn::retry(
                            accepted,
                        ))))
                        .map_err(supervisor_try_send),
                    Err(error) => Err(error),
                };
                let _ = completed.send(result);
            }
            ManagerCommand::WakeRetry {
                session_id,
                run_id,
                retrying_event_id,
                command_id,
                completed,
            } => {
                if let Some(supervisor) = supervisors.get(&session_id) {
                    if let Err(error) = supervisor.sender.try_send(SupervisorCommand::WakeRetry {
                        run_id,
                        retrying_event_id,
                        command_id,
                        completed,
                    }) {
                        let (completed, error) = match error {
                            mpsc::error::TrySendError::Full(SupervisorCommand::WakeRetry {
                                completed,
                                ..
                            }) => (completed, manager_busy("session worker queue is full")),
                            mpsc::error::TrySendError::Closed(SupervisorCommand::WakeRetry {
                                completed,
                                ..
                            }) => (completed, manager_stopped()),
                            _ => unreachable!(),
                        };
                        let _ = completed.send(Err(error));
                    }
                } else {
                    let _ = completed.send(Ok(()));
                }
            }
            ManagerCommand::Recover { mut pending } => {
                let session_id = pending.accepted.session_id.clone();
                match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    session_id,
                )
                .await
                {
                    Ok(supervisor) => {
                        if let Err(error) = supervisor.try_send(SupervisorCommand::Submit(pending))
                        {
                            let (mut pending, error) = match error {
                                mpsc::error::TrySendError::Full(SupervisorCommand::Submit(
                                    pending,
                                )) => (pending, manager_busy("recovered work queue is full")),
                                mpsc::error::TrySendError::Closed(SupervisorCommand::Submit(
                                    pending,
                                )) => (pending, manager_stopped()),
                                _ => unreachable!(),
                            };
                            if let Some(ready) = pending.recovery_ready.take() {
                                let result =
                                    terminalize_recovery_feed_failure(&hub, *pending, error).await;
                                let _ = ready.send(result);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(ready) = pending.recovery_ready.take() {
                            let result =
                                terminalize_recovery_feed_failure(&hub, *pending, error).await;
                            let _ = ready.send(result);
                        }
                    }
                }
            }
            ManagerCommand::Nudge {
                session_id,
                run_id,
                accepted_seq,
                text,
                mode,
                completed,
            } => {
                let supervisor = match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor,
                    Err(error) => {
                        let _ = completed.send(Err(error));
                        continue;
                    }
                };
                if let Err(error) = supervisor.try_send(SupervisorCommand::Nudge {
                    run_id,
                    accepted_seq,
                    text,
                    mode,
                    completed,
                }) {
                    let (completed, error) = match error {
                        mpsc::error::TrySendError::Full(SupervisorCommand::Nudge {
                            completed,
                            ..
                        }) => (completed, manager_busy("session supervisor queue is full")),
                        mpsc::error::TrySendError::Closed(SupervisorCommand::Nudge {
                            completed,
                            ..
                        }) => (completed, manager_stopped()),
                        _ => unreachable!(),
                    };
                    let _ = completed.send(Err(error));
                }
            }
            ManagerCommand::Compact {
                session_id,
                command_id,
                worker_generation,
                branch_id,
                completed,
            } => {
                let supervisor = match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    session_id,
                )
                .await
                {
                    Ok(supervisor) => supervisor,
                    Err(error) => {
                        let _ = completed.send(Err(error));
                        continue;
                    }
                };
                if let Err(error) = supervisor.try_send(SupervisorCommand::Compact {
                    command_id,
                    worker_generation,
                    branch_id,
                    completed,
                }) {
                    let (completed, error) = match error {
                        mpsc::error::TrySendError::Full(SupervisorCommand::Compact {
                            completed,
                            ..
                        }) => (completed, manager_busy("session supervisor queue is full")),
                        mpsc::error::TrySendError::Closed(SupervisorCommand::Compact {
                            completed,
                            ..
                        }) => (completed, manager_stopped()),
                        _ => unreachable!(),
                    };
                    let _ = completed.send(Err(error));
                }
            }
            ManagerCommand::ShellExec { pending, completed } => {
                let supervisor = match supervisor_for(
                    &hub,
                    &dependencies,
                    &drain_wakes,
                    &mut supervisors,
                    &mut tasks,
                    &mut task_sessions,
                    &mut incarnations,
                    pending.accepted.session_id.clone(),
                )
                .await
                {
                    Ok(supervisor) => supervisor,
                    Err(error) => {
                        let _ = completed.send(Err(error));
                        continue;
                    }
                };
                let result = supervisor
                    .try_send(SupervisorCommand::ShellExec(pending))
                    .map_err(supervisor_try_send);
                let _ = completed.send(result);
            }
            ManagerCommand::Shutdown { completed } => {
                for supervisor in supervisors.values() {
                    let _ = supervisor.sender.send(SupervisorCommand::Shutdown).await;
                }
                while let Some(outcome) = tasks.join_next_with_id().await {
                    handle_supervisor_exit(
                        &hub,
                        &mut supervisors,
                        &mut task_sessions,
                        &mut incarnations,
                        outcome,
                    )
                    .await;
                }
                let result = drain_accepted_without_handoff(&hub).await;
                let _ = completed.send(result);
                return;
            }
        }
    }
    for supervisor in supervisors.values() {
        let _ = supervisor.sender.send(SupervisorCommand::Shutdown).await;
    }
    while tasks.join_next().await.is_some() {}
}

async fn terminalize_recovery_feed_failure(
    hub: &SessionHub,
    pending: PendingTurn,
    error: HaiderError,
) -> Result<(), HaiderError> {
    let branch_id = pending.accepted.branch_id.clone();
    let run_id = pending.accepted.run_id;
    let session_id = pending.accepted.session_id;
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .map_err(hub_error)?;
    let device_id = DeviceId::new(format!(
        "recovery-feed-worker-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        run_id,
    ));
    let event_ids = EventIdGenerator::new(format!(
        "recovery-feed-event-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        run_id,
    ));
    let mut payloads = failed_resumption_payloads(&lease, &session_id, &run_id, &error).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    append_payloads(
        &lease,
        &device_id,
        &run_id,
        branch_id.as_ref(),
        &event_ids,
        payloads,
    )
    .await?;
    append_session_idle(&lease, &device_id, &event_ids, true).await?;
    let _ = lease.unregister_worker().await;
    tracing::warn!(
        %session_id,
        %run_id,
        ?error,
        "recovered work could not enter a supervisor and was terminalized"
    );
    Ok(())
}

async fn drain_accepted_without_handoff(hub: &SessionHub) -> Result<(), HaiderError> {
    let session_ids = hub.session_ids().await.map_err(hub_error)?;
    for session_id in session_ids {
        let lease = match hub
            .acquire_drain_worker_lease(session_id.clone())
            .await
            .map_err(hub_error)?
        {
            Some(lease) => lease,
            None => continue,
        };
        let device_id = DeviceId::new(format!(
            "drain-worker-{}-{}",
            session_id,
            lease.worker_generation()
        ));
        let event_ids = EventIdGenerator::new(format!(
            "drain-event-{}-{}",
            session_id,
            lease.worker_generation()
        ));
        let runs = durable_runs(&lease).await?;
        let candidates = runs
            .iter()
            .filter(|(_, state, _, _, _)| {
                !state.is_terminal()
                    && !matches!(
                        state,
                        RunState::InputRequired { .. }
                            | RunState::PermissionRequired { .. }
                            | RunState::Waiting {
                                reason: haider_protocol::state::WaitReason::LocalChild
                            }
                    )
            })
            .collect::<Vec<_>>();
        let reconciliation_targets = candidates
            .iter()
            .map(|(run_id, _, _, branch_id, _)| UnknownReconcileTarget {
                run_id,
                branch_id: branch_id.as_ref(),
            })
            .collect::<Vec<_>>();
        let effect_scans = scan_unknown_effects(&lease, &reconciliation_targets).await?;
        let mut terminalized = false;
        for ((run_id, state, _, branch_id, _), effect_scan) in
            candidates.into_iter().zip(effect_scans)
        {
            let user_command_scope = durable_user_command_scope(&lease, run_id).await?;
            // P3-4 (park, don't cancel): request-input, permission, and local
            // child checkpoints were excluded from this sweep above.
            if *state != RunState::Cancelling {
                if let Some(scope) = user_command_scope.as_ref() {
                    append_shell_payloads(
                        &lease,
                        &device_id,
                        run_id,
                        scope.branch_id.as_ref(),
                        scope.agent_id.as_ref(),
                        &event_ids,
                        vec![EventPayload::RunState(RunState::Cancelling)],
                    )
                    .await?;
                } else {
                    append_run_state(
                        &lease,
                        &device_id,
                        run_id,
                        branch_id.as_ref(),
                        &event_ids,
                        RunState::Cancelling,
                    )
                    .await?;
                }
            }
            let _ = append_unknown_effect_scan(
                &lease,
                &device_id,
                run_id,
                branch_id.as_ref(),
                &event_ids,
                UnknownReconcile::Cancel,
                effect_scan,
            )
            .await?;
            let mut payloads = cancelled_resumption_payloads(&lease, &session_id, run_id).await?;
            payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
            if let Some(scope) = user_command_scope {
                append_shell_payloads(
                    &lease,
                    &device_id,
                    run_id,
                    scope.branch_id.as_ref(),
                    scope.agent_id.as_ref(),
                    &event_ids,
                    payloads,
                )
                .await?;
            } else {
                append_payloads(
                    &lease,
                    &device_id,
                    run_id,
                    branch_id.as_ref(),
                    &event_ids,
                    payloads,
                )
                .await?;
            }
            terminalized = true;
        }
        if terminalized {
            append_session_idle(&lease, &device_id, &event_ids, true).await?;
        }
        lease.unregister_worker().await.map_err(hub_error)?;
    }
    Ok(())
}

async fn handle_supervisor_exit(
    hub: &SessionHub,
    supervisors: &mut HashMap<SessionId, SupervisorSlot>,
    task_sessions: &mut HashMap<tokio::task::Id, SessionId>,
    incarnations: &mut HashMap<SessionId, u64>,
    outcome: Result<(tokio::task::Id, SupervisorExit), tokio::task::JoinError>,
) {
    let (task_id, session_id, panicked, terminalize_nonterminal) = match outcome {
        Ok((task_id, exit)) => (
            task_id,
            exit.session_id,
            false,
            exit.terminalize_nonterminal,
        ),
        Err(error) => {
            let task_id = error.id();
            let Some(session_id) = task_sessions.get(&task_id).cloned() else {
                tracing::error!(?error, "unknown supervisor task failed");
                return;
            };
            (task_id, session_id, error.is_panic(), true)
        }
    };
    task_sessions.remove(&task_id);
    if supervisors
        .get(&session_id)
        .is_some_and(|slot| slot.task_id == task_id)
    {
        supervisors.remove(&session_id);
    }
    // EVICTION + INCARNATION are one law: eviction makes later submissions
    // usable again, while the next `supervisor_for` increments the retained
    // incarnation before constructing its EventIdGenerator. Never evict
    // without retaining this counter or a recreated supervisor could collide
    // with event IDs minted by its predecessor in the same store generation.
    let incarnation = *incarnations.entry(session_id.clone()).or_insert(1);
    if terminalize_nonterminal
        && let Err(error) = terminalize_supervisor_exit(hub, &session_id, incarnation).await
    {
        tracing::error!(
            %session_id,
            ?error,
            panicked,
            "exited supervisor work could not be terminalized"
        );
    }
}

pub(crate) async fn terminalize_supervisor_exit(
    hub: &SessionHub,
    session_id: &SessionId,
    incarnation: u64,
) -> Result<(), HaiderError> {
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .map_err(hub_error)?;
    let device_id = DeviceId::new(format!(
        "panic-worker-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        incarnation,
    ));
    let event_ids = EventIdGenerator::new(format!(
        "panic-event-{}-{}-{}",
        session_id,
        lease.worker_generation(),
        incarnation,
    ));
    let runs = durable_runs(&lease)
        .await?
        .into_iter()
        .filter(|(_, state, _, _, _)| !state.is_terminal())
        .collect::<Vec<_>>();
    let reconciliation_targets = runs
        .iter()
        .map(|(run_id, _, _, branch_id, _)| UnknownReconcileTarget {
            run_id,
            branch_id: branch_id.as_ref(),
        })
        .collect::<Vec<_>>();
    let effect_scans = scan_unknown_effects(&lease, &reconciliation_targets).await?;
    let mut terminalized = false;
    for ((run_id, state, _, branch_id, _), effect_scan) in runs.iter().zip(effect_scans) {
        let user_command_scope = durable_user_command_scope(&lease, run_id).await?;
        // Panic can strand a dispatched effect regardless of the run state.
        // Reconcile before either cancellation-shaped or failure-shaped
        // terminalization; a reconciliation error fences every terminal.
        let durable_state = append_unknown_effect_scan(
            &lease,
            &device_id,
            run_id,
            branch_id.as_ref(),
            &event_ids,
            UnknownReconcile::Park,
            effect_scan,
        )
        .await?;
        if durable_state == Some(RunState::EffectOutcomeUnknown) {
            // Unknown outcome is an intentional durable park with its own
            // four-choice card. Do not overwrite it with Errored/Idle.
            continue;
        }
        if *state == RunState::Cancelling {
            let mut payloads = cancelled_resumption_payloads(&lease, session_id, run_id).await?;
            payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
            if let Some(scope) = user_command_scope {
                append_shell_payloads(
                    &lease,
                    &device_id,
                    run_id,
                    scope.branch_id.as_ref(),
                    scope.agent_id.as_ref(),
                    &event_ids,
                    payloads,
                )
                .await?;
            } else {
                append_payloads(
                    &lease,
                    &device_id,
                    run_id,
                    branch_id.as_ref(),
                    &event_ids,
                    payloads,
                )
                .await?;
            }
            terminalized = true;
            continue;
        }
        let error = HaiderError::new(
            ErrorCode::Internal,
            "session supervisor exited before the run completed",
            true,
        );
        let mut payloads = failed_resumption_payloads(&lease, session_id, run_id, &error).await?;
        payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
        if let Some(scope) = user_command_scope {
            append_shell_payloads(
                &lease,
                &device_id,
                run_id,
                scope.branch_id.as_ref(),
                scope.agent_id.as_ref(),
                &event_ids,
                payloads,
            )
            .await?;
        } else {
            append_payloads(
                &lease,
                &device_id,
                run_id,
                branch_id.as_ref(),
                &event_ids,
                payloads,
            )
            .await?;
        }
        terminalized = true;
    }
    if terminalized {
        append_session_idle(&lease, &device_id, &event_ids, true).await?;
    }
    let _ = lease.unregister_worker().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn supervisor_for(
    hub: &SessionHub,
    dependencies: &WorkerDependencies,
    drain_wakes: &tokio::sync::watch::Receiver<bool>,
    supervisors: &mut HashMap<SessionId, SupervisorSlot>,
    tasks: &mut JoinSet<SupervisorExit>,
    task_sessions: &mut HashMap<tokio::task::Id, SessionId>,
    incarnations: &mut HashMap<SessionId, u64>,
    session_id: SessionId,
) -> Result<mpsc::Sender<SupervisorCommand>, HaiderError> {
    if let Some(supervisor) = supervisors.get(&session_id) {
        return if supervisor.sender.is_closed() {
            Err(manager_busy(
                "session supervisor is being evicted after exit",
            ))
        } else {
            Ok(supervisor.sender.clone())
        };
    }
    let metadata = hub.session_metadata(&session_id).await?.ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "legacy session has no live-worker metadata",
            false,
        )
    })?;
    let (cancellation_wake, cancellation_wakes) = tokio::sync::watch::channel(0_u64);
    let lease = hub
        .acquire_worker_lease_with_wakes(session_id.clone(), cancellation_wake)
        .await
        .map_err(hub_error)?;
    let (sender, receiver) = mpsc::channel(SUPERVISOR_CAPACITY);
    let incarnation = *incarnations
        .entry(session_id.clone())
        .and_modify(|incarnation| *incarnation = incarnation.saturating_add(1))
        .or_insert(1);
    let task_session_id = session_id.clone();
    let supervisor_dependencies = dependencies.clone();
    let supervisor_drain_wakes = (*drain_wakes).clone();
    let task = tasks.spawn(async move {
        let terminalize_nonterminal = run_supervisor(
            supervisor_dependencies,
            metadata,
            lease,
            receiver,
            cancellation_wakes,
            supervisor_drain_wakes,
            incarnation,
        )
        .await;
        SupervisorExit {
            session_id: task_session_id,
            terminalize_nonterminal,
        }
    });
    let task_id = task.id();
    task_sessions.insert(task_id, session_id.clone());
    supervisors.insert(
        session_id,
        SupervisorSlot {
            sender: sender.clone(),
            task_id,
        },
    );
    Ok(sender)
}

struct ActiveTurn {
    run_id: RunId,
    branch_id: Option<BranchId>,
    cancel: CancelToken,
    outcome: Pin<Box<dyn FutureTurn>>,
    harness: haider_core::HarnessHandle,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    actor: Option<JoinHandle<()>>,
    /// W-B: whether this turn declared the anthropic SERVER web tools —
    /// the precondition for the invalid-request degrade latch.
    anthropic_web_tools: bool,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        if let Some(actor) = &self.actor {
            actor.abort();
        }
    }
}

/// Exact discriminator for a hosted-web capability refusal that escaped the
/// same-turn local fallback. Generic invalid requests must never silently
/// disable provider functionality on a later turn.
fn is_hosted_web_tool_rejection(error: &HaiderError) -> bool {
    error
        .presentation
        .as_ref()
        .is_some_and(|presentation| presentation.subcode.as_str() == "provider-web-tool-rejected")
}

trait FutureTurn:
    std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}
impl<T> FutureTurn for T where
    T: std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}

/// One session's turn loop: strictly serial turns from the bounded queue,
/// with four live inputs while a turn runs — submissions (queued behind the
/// active run), the hub's cancellation wake (durable `Cancelling` reconciled
/// from the journal, active token cancelled), the actor-ordered promoted-steer
/// lane, and the active turn's outcome
/// (dispatcher closed, harness stopped and joined, then the store-side
/// conditional Idle settle — `Store::settle_session_idle` owns that law).
/// Shutdown cancels ordinary active turns, but silently parks a durable input,
/// permission, or local-child checkpoint for startup recovery. Queued runs are
/// terminalized and the supervisor deregisters its lease on the way out.
async fn run_supervisor(
    dependencies: WorkerDependencies,
    metadata: SessionMetadataV1,
    lease: HubStoreHandle,
    mut commands: mpsc::Receiver<SupervisorCommand>,
    mut cancellation_wakes: tokio::sync::watch::Receiver<u64>,
    mut drain_wakes: tokio::sync::watch::Receiver<bool>,
    incarnation: u64,
) -> bool {
    let mut queue = VecDeque::<PendingTurn>::new();
    let mut active: Option<ActiveTurn> = None;
    let device_id = DeviceId::new(format!(
        "worker-{}-{}-{}",
        lease.session_id(),
        lease.worker_generation(),
        incarnation,
    ));
    let event_ids = Arc::new(EventIdGenerator::new(format!(
        "worker-event-{}-{}-{}",
        lease.session_id(),
        lease.worker_generation(),
        incarnation,
    )));
    let mut stopping = false;
    let mut parked_checkpoint = false;
    let mut rescan_needed = false;
    let mut deferred_shell = VecDeque::<PendingShellExec>::new();
    // A nudge's accepted user-message sequence is its process-stable delivery
    // key. Existing messages are already part of a restarted turn's compiled
    // prompt; new messages are inserted when they cross into the live harness.
    let mut delivered_nudges = durable_user_message_seqs(&lease).await.unwrap_or_default();
    // W-A: rebuild this session's background-task projection and reap
    // prior-generation orphans as soon as the session becomes live again.
    {
        let tasks = crate::tasks::TaskFacade::new(lease.hub().clone());
        let session_id = lease.session_id().clone();
        tokio::spawn(async move {
            if let Err(error) = tasks.adopt_session(&session_id).await {
                tracing::warn!(%session_id, ?error, "background-task adoption failed at supervisor start");
            }
        });
    }

    loop {
        if active.is_none() && !stopping {
            if let Some(pending) = deferred_shell.pop_front() {
                if let Err(error) = perform_shell_exec(
                    &metadata,
                    &lease,
                    &device_id,
                    Arc::clone(&event_ids),
                    dependencies.diagnostics.clone(),
                    pending,
                    &mut cancellation_wakes,
                    &mut drain_wakes,
                )
                .await
                {
                    tracing::error!(
                        session_id = %lease.session_id(),
                        ?error,
                        "deferred direct shell execution failed before terminal settlement"
                    );
                    let _ = lease.unregister_worker().await;
                    return true;
                }
                continue;
            }
            if queue.is_empty() && rescan_needed {
                rescan_needed = refill_queued_turns(&lease, &mut queue, None).await;
            }
            let direct_shell_owns_session = durable_runs(&lease).await.is_ok_and(|runs| {
                runs.into_iter()
                    .any(|(_, state, _, _, _)| state == RunState::RunningTool)
            });
            while !direct_shell_owns_session && let Some(pending) = queue.pop_front() {
                let mut pending = pending;
                let run_id = pending.accepted.run_id.clone();
                let branch_id = pending.accepted.branch_id.clone();
                let mut recovery_ready = pending.recovery_ready.take();
                let recovering = pending.recovering;
                if pending.accepted.disposition == haider_core::TurnAdmissionDisposition::Queued {
                    match lease
                        .consume_queued_turn(run_id.clone(), event_ids.next(), device_id.clone())
                        .await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) => match durable_queue_consumed(&lease, &run_id).await {
                            Ok(true) => {
                                // Crash recovery after the durable consumption
                                // boundary: the row is already gone, but this
                                // exact queued run still owns delivery and must
                                // continue into start_turn without a second
                                // delta.
                            }
                            Ok(false) => {
                                // A fenced remove/promote won the same durable
                                // race. This stale in-memory hint must never
                                // start.
                                if let Some(ready) = recovery_ready.take() {
                                    let _ = ready.send(Ok(()));
                                }
                                continue;
                            }
                            Err(error) => {
                                tracing::error!(
                                    session_id = %lease.session_id(),
                                    %run_id,
                                    ?error,
                                    "queued turn consumption truth could not be recovered"
                                );
                                if let Some(ready) = recovery_ready.take() {
                                    let _ = ready.send(Err(error));
                                }
                                let _ = lease.unregister_worker().await;
                                return false;
                            }
                        },
                        Err(error) => {
                            tracing::error!(
                                session_id = %lease.session_id(),
                                %run_id,
                                ?error,
                                "queued turn could not cross its durable consumption boundary"
                            );
                            if let Some(ready) = recovery_ready.take() {
                                let _ = ready.send(Err(error));
                            }
                            let _ = lease.unregister_worker().await;
                            return false;
                        }
                    }
                }
                // R6 extension (F1): the pair is re-read per logical turn, so
                // a committed `session.select_model` reaches the NEXT turn
                // without a worker restart. A failed read fails the turn
                // honestly instead of silently pinning the spawn snapshot.
                let turn_result = match fresh_turn_metadata(&lease).await {
                    Ok(fresh) => {
                        start_turn(
                            &dependencies,
                            &fresh,
                            &lease,
                            &device_id,
                            Arc::clone(&event_ids),
                            pending,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                match turn_result {
                    Ok(turn) => {
                        if let Some(ready) = recovery_ready {
                            let _ = ready.send(Ok(()));
                        }
                        active = Some(turn);
                        break;
                    }
                    Err(error) => {
                        if matches!(
                            durable_run_state(&lease, &run_id).await,
                            Some(RunState::Cancelling | RunState::Cancelled)
                        ) {
                            let terminalized = match cancelled_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &run_id,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    match append_payloads(
                                        &lease,
                                        &device_id,
                                        &run_id,
                                        branch_id.as_ref(),
                                        &event_ids,
                                        payloads,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            append_session_idle(
                                                &lease, &device_id, &event_ids, true,
                                            )
                                            .await
                                        }
                                        Err(error) => Err(error),
                                    }
                                }
                                Err(error) => Err(error),
                            };
                            if let Some(ready) = recovery_ready {
                                let _ = ready.send(terminalized);
                            }
                        } else if recovering {
                            let terminalized = match failed_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &run_id,
                                &error,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    match append_payloads(
                                        &lease,
                                        &device_id,
                                        &run_id,
                                        branch_id.as_ref(),
                                        &event_ids,
                                        payloads,
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            append_session_idle(
                                                &lease, &device_id, &event_ids, true,
                                            )
                                            .await
                                        }
                                        Err(error) => Err(error),
                                    }
                                }
                                Err(terminalize_error) => Err(terminalize_error),
                            };
                            tracing::warn!(
                                session_id = %lease.session_id(),
                                %run_id,
                                ?error,
                                "recovered work could not resume and was terminalized"
                            );
                            if let Some(ready) = recovery_ready {
                                let _ = ready.send(terminalized);
                            }
                        } else {
                            let _ = append_failure(
                                &lease,
                                &device_id,
                                &run_id,
                                branch_id.as_ref(),
                                &event_ids,
                                error,
                            )
                            .await;
                            let _ =
                                append_session_idle(&lease, &device_id, &event_ids, false).await;
                        }
                    }
                }
            }
        }

        if stopping && active.is_none() {
            break;
        }

        if let Some(turn) = active.as_mut() {
            let active_run = turn.run_id.clone();
            let active_cancel = turn.cancel.clone();
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(SupervisorCommand::Submit(pending)) => {
                            admit_pending(
                                &mut queue,
                                &mut rescan_needed,
                                &lease,
                                &device_id,
                                &event_ids,
                                Some(&active_run),
                                *pending,
                            ).await;
                        }
                        Some(SupervisorCommand::WakeRetry {
                            run_id,
                            retrying_event_id,
                            command_id,
                            completed,
                        }) => {
                            if run_id == active_run {
                                let _ = turn
                                    .harness
                                    .wake_provider_retry(command_id, &retrying_event_id);
                            }
                            // A natural timer/attempt/terminal transition may
                            // have won after durable receipt acceptance. That
                            // is the fulfilled idempotent no-op case.
                            let _ = completed.send(Ok(()));
                        }
                        Some(SupervisorCommand::Nudge {
                            run_id,
                            accepted_seq,
                            text,
                            mode,
                            completed,
                        }) => {
                            let result = deliver_mid_turn_to_active(
                                turn,
                                &active_run,
                                &mut delivered_nudges,
                                run_id,
                                accepted_seq,
                                text,
                                mode,
                            );
                            let _ = completed.send(result);
                        }
                        Some(SupervisorCommand::Compact { completed, .. }) => {
                            let _ = completed.send(Err(HaiderError::new(
                                ErrorCode::Busy,
                                "manual context compaction is idle-only",
                                true,
                            )));
                        }
                        Some(SupervisorCommand::ShellExec(pending)) => {
                            // Store admission can observe the turn's durable
                            // terminal a few instructions before this branch
                            // reaps `ActiveTurn`. Preserve that accepted job
                            // for the next loop instead of dropping its only
                            // handoff. Receipt replays for the same run are
                            // response-only duplicates and need one slot.
                            defer_shell_handoff(&mut deferred_shell, *pending);
                        }
                        Some(SupervisorCommand::Shutdown) | None => {
                            stopping = true;
                            // P3-4/W6c (park, don't cancel): request-input and
                            // local-child waits are durable checkpoints. An
                            // active delegated child is also preserved: its
                            // recovered parent re-arms supervision from the
                            // last committed envelope instead of a graceful
                            // restart silently becoming child cancellation.
                            let delegated_child = match dependencies.delegation.as_ref() {
                                Some(delegation) => delegation
                                    .agent_for_session(lease.session_id())
                                    .await
                                    .is_ok_and(|agent| agent.is_some()),
                                None => false,
                            };
                            if delegated_child || matches!(
                                durable_run_state(&lease, &active_run).await,
                                Some(
                                    RunState::InputRequired { .. }
                                    | RunState::PermissionRequired { .. },
                                )
                                    | Some(RunState::Waiting {
                                        reason: haider_protocol::state::WaitReason::LocalChild
                                    })
                            ) {
                                if let Some(parked) = active.take() {
                                    park_request_input_checkpoint(parked).await;
                                    parked_checkpoint = true;
                                }
                            } else {
                                active_cancel.cancel();
                            }
                            cancel_durable_queued_turns(
                                &mut queue,
                                &lease,
                                &device_id,
                                &event_ids,
                                Some(&active_run),
                            )
                            .await;
                        }
                    }
                }
                changed = cancellation_wakes.changed() => {
                    if changed.is_ok() {
                        reconcile_durable_cancellations(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            Some((&active_run, &active_cancel)),
                        ).await;
                    }
                }
                outcome = turn.outcome.as_mut() => {
                    if let Some(mut finished) = active.take() {
                        let (outcome_state, outcome_error, drive_error) = match outcome {
                            Ok(outcome) => (Some(outcome.state), outcome.error, None),
                            Err(error) => (None, None, Some(error)),
                        };
                        // The core actor performs the normal exact refusal
                        // fallback in this SAME turn. This latch is only the
                        // terminal safety net if that exact typed refusal
                        // escapes before fallback can be installed. A generic
                        // invalid request never disables hosted tools.
                        if finished.anthropic_web_tools
                            && outcome_error
                                .as_ref()
                                .or(drive_error.as_ref())
                                .is_some_and(is_hosted_web_tool_rejection)
                        {
                            lease.hub().degrade_anthropic_web_tools(lease.session_id());
                        }
                        let cancellation_requested = matches!(
                            outcome_state.as_ref(),
                            Some(RunState::Cancelled)
                        ) || durable_run_state(&lease, &finished.run_id).await
                            == Some(RunState::Cancelling);
                        if let Some(dispatcher) = finished.dispatcher.take()
                            && let Err(error) = if cancellation_requested {
                                dispatcher.cancel().await
                            } else {
                                dispatcher.close().await
                            }
                        {
                            tracing::warn!(run_id = %finished.run_id, ?error, "turn tool dispatcher close failed");
                        }
                        let _ = finished.harness.stop().await;
                        let actor_panicked = if let Some(actor) = finished.actor.take() {
                            actor.await.is_err()
                        } else {
                            false
                        };
                        if drive_error.is_some() || actor_panicked {
                            let error = drive_error.unwrap_or_else(|| {
                                HaiderError::new(
                                    ErrorCode::Internal,
                                    "turn harness actor panicked",
                                    true,
                                )
                            });
                            let durable = match reconcile_unknown_effects(
                                &lease,
                                &device_id,
                                &finished.run_id,
                                finished.branch_id.as_ref(),
                                &event_ids,
                                if cancellation_requested {
                                    UnknownReconcile::Cancel
                                } else {
                                    UnknownReconcile::Park
                                },
                            )
                            .await
                            {
                                Ok(durable) => durable,
                                Err(reconcile_error) => {
                                    tracing::error!(
                                        run_id = %finished.run_id,
                                        ?reconcile_error,
                                        "failed turn effect reconciliation blocked terminal commit"
                                    );
                                    let _ = lease.unregister_worker().await;
                                    return false;
                                }
                            };
                            if durable == Some(RunState::EffectOutcomeUnknown) {
                                let _ = lease.unregister_worker().await;
                                return true;
                            }
                            match failed_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &finished.run_id,
                                &error,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    if append_payloads(
                                        &lease,
                                        &device_id,
                                        &finished.run_id,
                                        finished.branch_id.as_ref(),
                                        &event_ids,
                                        payloads,
                                    )
                                    .await
                                    .is_ok()
                                    {
                                        let _ = append_session_idle(
                                            &lease, &device_id, &event_ids, true,
                                        )
                                        .await;
                                    }
                                }
                                Err(terminalize_error) => {
                                    tracing::error!(
                                        run_id = %finished.run_id,
                                        ?terminalize_error,
                                        "failed turn could not be terminalized"
                                    );
                                }
                            }
                            // Returning is intentional: the manager observes
                            // this JoinSet exit, evicts the slot, and retains
                            // the incarnation counter before a later submit.
                            let _ = lease.unregister_worker().await;
                            return true;
                        }
                        // TERMINAL ORDER: core cancellation is deliberately
                        // non-terminal in daemon mode. Orderly cancellation
                        // closes every abandoned dispatch as Cancelled before
                        // the run itself crosses its terminal boundary.
                        let durable = match reconcile_unknown_effects(
                            &lease,
                            &device_id,
                            &finished.run_id,
                            finished.branch_id.as_ref(),
                            &event_ids,
                            if cancellation_requested {
                                UnknownReconcile::Cancel
                            } else {
                                UnknownReconcile::EvidenceOnly
                            },
                        )
                        .await
                        {
                            Ok(durable) => durable,
                            Err(error) => {
                                // Never cross the terminal boundary while a
                                // Dispatched effect still lacks an outcome. A
                                // later startup/fresh supervisor may reconcile
                                // it, but this exit must not synthesize Cancelled.
                                tracing::error!(
                                    run_id = %finished.run_id,
                                    ?error,
                                    "effect reconciliation failed; terminal commit remains fenced"
                                );
                                let _ = lease.unregister_worker().await;
                                return false;
                            }
                        };
                        if durable == Some(RunState::EffectOutcomeUnknown) {
                            let _ = lease.unregister_worker().await;
                            return true;
                        }
                        let cancelled =
                            idle_interrupted_after_outcome(outcome_state.as_ref(), durable.as_ref());
                        if cancelled {
                            // Reduce durable lifecycle truth again after the
                            // harness stops. If core cancellation itself
                            // failed while closing an item/menu, finish those
                            // objects before the terminal boundary.
                            match cancelled_resumption_payloads(
                                &lease,
                                lease.session_id(),
                                &finished.run_id,
                            )
                            .await
                            {
                                Ok(mut payloads) => {
                                    payloads.retain(|payload| {
                                        !matches!(payload, EventPayload::SessionState(_))
                                    });
                                    let _ = append_payloads(
                                        &lease,
                                        &device_id,
                                        &finished.run_id,
                                        finished.branch_id.as_ref(),
                                        &event_ids,
                                        payloads,
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    tracing::error!(
                                        run_id = %finished.run_id,
                                        ?error,
                                        "cancellation lifecycle reduction failed; terminal remains fenced"
                                    );
                                }
                            }
                        }
                        // Natural completion remains plain Idle even when it
                        // wins a drain race. Cancellation (user or drain) owns
                        // the interrupted marker.
                        let _ = append_session_idle(
                            &lease,
                            &device_id,
                            &event_ids,
                            cancelled,
                        )
                        .await;
                    }
                }
            }
        } else {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(SupervisorCommand::Submit(pending)) => {
                        admit_pending(
                            &mut queue,
                            &mut rescan_needed,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                            *pending,
                        ).await;
                    }
                    Some(SupervisorCommand::WakeRetry { completed, .. }) => {
                        // The backoff ended between receipt commit and worker
                        // delivery. Never create/restart a run here.
                        let _ = completed.send(Ok(()));
                    }
                    Some(SupervisorCommand::Nudge { completed, .. }) => {
                        let _ = completed.send(Err(HaiderError::new(
                            ErrorCode::RunNotActive,
                            "daemon steer found no active run",
                            false,
                        )));
                    }
                    Some(SupervisorCommand::Compact {
                        command_id,
                        worker_generation,
                        branch_id,
                        completed,
                    }) => {
                        // Manual compaction is provider work between turns:
                        // it follows the CURRENT model selection exactly like
                        // the next turn would (F1).
                        let result = match fresh_turn_metadata(&lease).await {
                            Ok(fresh) => {
                                perform_manual_compaction(
                                    &dependencies,
                                    &fresh,
                                    &lease,
                                    &device_id,
                                    Arc::clone(&event_ids),
                                    command_id,
                                    worker_generation,
                                    branch_id,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        };
                        let _ = completed.send(result);
                    }
                    Some(SupervisorCommand::ShellExec(pending)) => {
                        if let Err(error) = perform_shell_exec(
                            &metadata,
                            &lease,
                            &device_id,
                            Arc::clone(&event_ids),
                            dependencies.diagnostics.clone(),
                            *pending,
                            &mut cancellation_wakes,
                            &mut drain_wakes,
                        )
                        .await
                        {
                            tracing::error!(
                                session_id = %lease.session_id(),
                                ?error,
                                "direct shell execution failed before terminal settlement"
                            );
                            let _ = lease.unregister_worker().await;
                            return true;
                        }
                    }
                    Some(SupervisorCommand::Shutdown) | None => {
                        stopping = true;
                        let last = cancel_durable_queued_turns(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                        ).await;
                        if last.is_some() {
                            let _ = append_session_idle(&lease, &device_id, &event_ids, true)
                                .await;
                        }
                    }
                },
                changed = cancellation_wakes.changed() => {
                    if changed.is_ok() {
                        reconcile_durable_cancellations(
                            &mut queue,
                            &lease,
                            &device_id,
                            &event_ids,
                            None,
                        ).await;
                    }
                }
            }
        }
    }
    let _ = lease.unregister_worker().await;
    !parked_checkpoint
}

fn deliver_mid_turn_to_active(
    turn: &mut ActiveTurn,
    active_run: &RunId,
    delivered_nudges: &mut HashSet<u64>,
    run_id: RunId,
    accepted_seq: u64,
    text: String,
    mode: DeliveryMode,
) -> Result<(), HaiderError> {
    if &run_id != active_run {
        return Err(HaiderError::new(
            ErrorCode::RunNotActive,
            "daemon steer targeted a different active run",
            false,
        ));
    }
    if delivered_nudges.contains(&accepted_seq) {
        return Ok(());
    }
    let result = match mode {
        DeliveryMode::Steer => turn.harness.nudge(text),
        DeliveryMode::Subturn => turn.harness.subturn(text),
        DeliveryMode::Queue => Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            "queue-mode input cannot target an active harness",
            false,
        )),
    };
    if result.is_ok() {
        delivered_nudges.insert(accepted_seq);
    }
    result
}

/// Chooses the one `Idle { interrupted }` meaning after a live outcome.
///
/// Drain state is intentionally absent: a turn whose durable terminal is
/// natural `Done` settles `false` even if drain began before its supervisor
/// observed the outcome. Only cancellation-shaped truth settles `true`.
fn idle_interrupted_after_outcome(
    outcome_state: Option<&RunState>,
    durable_state: Option<&RunState>,
) -> bool {
    matches!(outcome_state, Some(RunState::Cancelled))
        || matches!(durable_state, Some(RunState::Cancelling))
}

async fn admit_pending(
    queue: &mut VecDeque<PendingTurn>,
    rescan_needed: &mut bool,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active_run: Option<&RunId>,
    mut pending: PendingTurn,
) {
    if pending.checkpoint.is_some() || pending.child_wait.is_some() {
        if queue.len() < SUPERVISOR_CAPACITY {
            queue.push_back(pending);
        } else if let Some(ready) = pending.recovery_ready.take() {
            let _ = ready.send(Err(HaiderError::new(
                ErrorCode::Busy,
                "recovered checkpoint could not enter the bounded supervisor queue",
                true,
            )));
        }
        return;
    }
    let run_id = pending.accepted.run_id.clone();
    let branch_id = pending.accepted.branch_id.clone();
    let state = durable_runs(store).await.ok().and_then(|runs| {
        runs.into_iter()
            .find_map(|(candidate, state, _, _, _)| (candidate == run_id).then_some(state))
    });
    match state {
        Some(RunState::Queued) => {
            if active_run == Some(&run_id)
                || queue.iter().any(|queued| queued.accepted.run_id == run_id)
            {
                if let Some(ready) = pending.recovery_ready.take() {
                    let _ = ready.send(Ok(()));
                }
                return;
            }
            if queue.len() < SUPERVISOR_CAPACITY {
                if let Some(ready) = pending.recovery_ready.take() {
                    // Handoff, not provider entry, is the Ready boundary for
                    // queued recovery. An earlier recovered checkpoint can
                    // remain parked indefinitely while this durable run waits
                    // safely in the owned supervisor.
                    let _ = ready.send(Ok(()));
                }
                queue.push_back(pending);
            } else if pending.recovering {
                let error = HaiderError::new(
                    ErrorCode::Busy,
                    "recovered queued turn exceeded the bounded supervisor queue",
                    true,
                );
                let terminalized =
                    match failed_resumption_payloads(store, store.session_id(), &run_id, &error)
                        .await
                    {
                        Ok(mut payloads) => {
                            payloads.retain(|payload| {
                                !matches!(payload, EventPayload::SessionState(_))
                            });
                            match append_payloads(
                                store,
                                device_id,
                                &run_id,
                                branch_id.as_ref(),
                                event_ids,
                                payloads,
                            )
                            .await
                            {
                                Ok(()) => {
                                    append_session_idle(store, device_id, event_ids, true).await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    };
                if let Some(ready) = pending.recovery_ready.take() {
                    let _ = ready.send(terminalized);
                }
            } else {
                // The durable Queued/UserMessage pair is the overflow buffer.
                // A later completion refills from the journal.
                *rescan_needed = true;
            }
        }
        Some(RunState::Cancelling) => {
            let terminalized =
                match cancelled_resumption_payloads(store, store.session_id(), &run_id).await {
                    Ok(mut payloads) => {
                        payloads
                            .retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
                        match append_payloads(
                            store,
                            device_id,
                            &run_id,
                            branch_id.as_ref(),
                            event_ids,
                            payloads,
                        )
                        .await
                        {
                            Ok(()) => append_session_idle(store, device_id, event_ids, true).await,
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
            if let Some(ready) = pending.recovery_ready.take() {
                let _ = ready.send(terminalized);
            }
        }
        _ => {
            // Receipt replays for active or terminal runs are response-only.
            if let Some(ready) = pending.recovery_ready.take() {
                let _ = ready.send(Ok(()));
            }
        }
    }
}

async fn refill_queued_turns(
    store: &HubStoreHandle,
    queue: &mut VecDeque<PendingTurn>,
    active_run: Option<&RunId>,
) -> bool {
    let Ok(runs) = durable_runs(store).await else {
        return true;
    };
    let mut more = false;
    for (run_id, state, accepted_seq, branch_id, prompt_run_id) in runs {
        if state != RunState::Queued
            || active_run == Some(&run_id)
            || queue
                .iter()
                .any(|pending| pending.accepted.run_id == run_id)
        {
            continue;
        }
        if queue.len() >= SUPERVISOR_CAPACITY {
            more = true;
            continue;
        }
        let Some(accepted_seq) = accepted_seq else {
            continue;
        };
        let mut pending = PendingTurn::accepted(AcceptedTurn {
            session_id: store.session_id().clone(),
            run_id,
            accepted_seq,
            worker_generation: store.worker_generation(),
            branch_id,
            disposition: haider_core::TurnAdmissionDisposition::Queued,
            first_user_turn: false,
            pdf_attachments: Vec::new(),
        });
        pending.prompt_run_id = prompt_run_id;
        queue.push_back(pending);
    }
    more
}

async fn reconcile_durable_cancellations(
    queue: &mut VecDeque<PendingTurn>,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active: Option<(&RunId, &CancelToken)>,
) {
    let Ok(runs) = durable_runs(store).await else {
        return;
    };
    let active_run = active.map(|(run_id, _)| run_id);
    let mut terminalized = Vec::new();
    for (run_id, state, _, branch_id, _) in runs {
        if state != RunState::Cancelling {
            continue;
        }
        if active_run == Some(&run_id) {
            if let Some((_, cancel)) = active {
                cancel.cancel();
            }
            continue;
        }
        match durable_user_command_scope(store, &run_id).await {
            Ok(Some(scope)) => {
                let payloads = cancelled_resumption_payloads(store, store.session_id(), &run_id)
                    .await
                    .map(|mut payloads| {
                        payloads
                            .retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
                        payloads
                    });
                if let Ok(payloads) = payloads
                    && append_shell_payloads(
                        store,
                        device_id,
                        &run_id,
                        scope.branch_id.as_ref(),
                        scope.agent_id.as_ref(),
                        event_ids,
                        payloads,
                    )
                    .await
                    .is_ok()
                {
                    terminalized.push(run_id);
                }
            }
            Ok(None) => {
                if append_run_state(
                    store,
                    device_id,
                    &run_id,
                    branch_id.as_ref(),
                    event_ids,
                    RunState::Cancelled,
                )
                .await
                .is_ok()
                {
                    terminalized.push(run_id);
                }
            }
            Err(error) => {
                tracing::error!(%run_id, ?error, "direct shell cancellation scope is corrupt");
            }
        }
    }
    queue.retain(|pending| !terminalized.contains(&pending.accepted.run_id));
    if !terminalized.is_empty() {
        let _ = append_session_idle(store, device_id, event_ids, true).await;
    }
}

/// Reduces the committed journal to `(run, latest state, accepted seq)` in
/// acceptance order — the durable truth every admission/cancellation/refill
/// decision reads instead of trusting in-memory hints (module charter).
/// Its intentional O(journal) cost and projection trigger are ledgered in
/// `docs/OPTIMIZATIONS.md` under W3c1.
async fn durable_runs(
    store: &HubStoreHandle,
) -> Result<
    Vec<(
        RunId,
        RunState,
        Option<u64>,
        Option<BranchId>,
        Option<RunId>,
    )>,
    HaiderError,
> {
    let mut cursor = 0;
    let mut runs =
        HashMap::<RunId, (RunState, Option<u64>, Option<BranchId>, Option<RunId>)>::new();
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Some(run_id) = envelope.run_id else {
                continue;
            };
            if let Ok(RunRetryEventPayload::RunRetried { prompt_run_id, .. }) =
                RunRetryEventPayload::from_payload_value(envelope.payload.clone())
            {
                let (state, branch_id) = runs.get(&run_id).map_or(
                    (RunState::Queued, envelope.branch_id.clone()),
                    |(state, _, branch_id, _)| (state.clone(), branch_id.clone()),
                );
                if branch_id != envelope.branch_id {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("run {run_id} crosses branch scopes"),
                        false,
                    ));
                }
                runs.insert(
                    run_id,
                    (state, Some(envelope.seq), branch_id, Some(prompt_run_id)),
                );
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::RunState(state) => {
                    let (accepted, branch_id, prompt_run_id) = runs.get(&run_id).map_or(
                        (None, envelope.branch_id.clone(), None),
                        |(_, seq, branch_id, prompt_run_id)| {
                            (*seq, branch_id.clone(), prompt_run_id.clone())
                        },
                    );
                    if branch_id != envelope.branch_id {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} crosses branch scopes"),
                            false,
                        ));
                    }
                    runs.insert(run_id, (state, accepted, branch_id, prompt_run_id));
                }
                EventPayload::UserMessage { .. } => {
                    let (state, branch_id, prompt_run_id) = runs.get(&run_id).map_or(
                        (RunState::Queued, envelope.branch_id.clone(), None),
                        |(state, _, branch_id, prompt_run_id)| {
                            (state.clone(), branch_id.clone(), prompt_run_id.clone())
                        },
                    );
                    if branch_id != envelope.branch_id {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} crosses branch scopes"),
                            false,
                        ));
                    }
                    runs.insert(
                        run_id,
                        (state, Some(envelope.seq), branch_id, prompt_run_id),
                    );
                }
                _ => {}
            }
        }
    }
    let mut runs = runs
        .into_iter()
        .map(|(run_id, (state, accepted, branch_id, prompt_run_id))| {
            (run_id, state, accepted, branch_id, prompt_run_id)
        })
        .collect::<Vec<_>>();
    runs.sort_by_key(|(_, _, accepted, _, _)| accepted.unwrap_or(u64::MAX));
    Ok(runs)
}

/// Returns the daemon-minted prompt scope for a direct user command run.
/// Model tool runs also use `RunningTool`, so only the hidden, item-linked
/// origin marker may select shell-specific recovery shaping.
async fn durable_user_command_scope(
    store: &HubStoreHandle,
    run_id: &RunId,
) -> Result<Option<DurableUserCommandScope>, HaiderError> {
    let mut cursor = 0;
    let mut marker = None::<(UserCommandOriginV1, Option<BranchId>, Option<AgentId>)>;
    let mut command_items = HashMap::<ItemId, (String, Option<BranchId>, Option<AgentId>)>::new();
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(EventPayload::Item(item_event)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            match item_event {
                ItemEvent::Started {
                    item_id,
                    item: TurnItem::CommandExecution { call_id, .. },
                } => {
                    let previous = command_items
                        .insert(item_id, (call_id, envelope.branch_id, envelope.agent_id));
                    if previous.is_some() {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} starts the same command item twice"),
                            false,
                        ));
                    }
                }
                ItemEvent::Completed { item, .. } => {
                    let origin =
                        UserCommandOriginV1::try_from_extension_item(&item).map_err(|error| {
                            HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                format!("run {run_id} has malformed user-command origin: {error}"),
                                false,
                            )
                        })?;
                    if let Some(origin) = origin
                        && marker
                            .replace((origin, envelope.branch_id, envelope.agent_id))
                            .is_some()
                    {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} has duplicate user-command origins"),
                            false,
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    let Some((origin, branch_id, agent_id)) = marker else {
        return Ok(None);
    };
    let Some((call_id, item_branch_id, item_agent_id)) = command_items.get(&origin.command_item_id)
    else {
        return Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("run {run_id} user-command origin points to a missing command item"),
            false,
        ));
    };
    if call_id != &origin.call_id || item_branch_id != &branch_id || item_agent_id != &agent_id {
        return Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("run {run_id} user-command origin does not match its command item"),
            false,
        ));
    }
    Ok(Some(DurableUserCommandScope {
        branch_id,
        agent_id,
    }))
}

async fn durable_user_message_seqs(store: &HubStoreHandle) -> Result<HashSet<u64>, HaiderError> {
    let mut cursor = 0;
    let mut sequences = HashSet::new();
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(sequences);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if serde_json::from_value::<EventPayload>(envelope.payload)
                .is_ok_and(|payload| matches!(payload, EventPayload::UserMessage { .. }))
            {
                sequences.insert(envelope.seq);
            }
        }
    }
}

/// How a caller settles dispatched effects that lack a durable outcome.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnknownReconcile {
    /// Death-shaped exits (supervisor panic, harness actor death): nobody is
    /// left to settle the run, so append Unknown evidence, open the
    /// four-choice recovery card, and park the run as `EffectOutcomeUnknown`.
    Park,
    /// Failure terminalization appends Unknown evidence only — no card and no
    /// park — before the caller commits its own `Errored` terminal.
    EvidenceOnly,
    /// Orderly cancellation: a dispatch with no outcome was deliberately
    /// abandoned and is terminalized as `Cancelled`, never a crash window.
    Cancel,
}

#[derive(Default)]
struct UnknownEffectScan {
    dispatched: HashSet<EffectId>,
    summaries: HashMap<EffectId, String>,
    /// `true` is Unknown; `false` is any terminal outcome.
    outcomes: HashMap<EffectId, bool>,
    open_recovery: HashMap<MenuId, EffectId>,
    durable_state: Option<RunState>,
    durable_state_branch: Option<BranchId>,
    durable_state_seen: bool,
    durable_state_corrupt: bool,
}

#[derive(Clone, Copy)]
struct UnknownReconcileTarget<'a> {
    run_id: &'a RunId,
    branch_id: Option<&'a BranchId>,
}

impl UnknownEffectScan {
    fn observe_durable_state(&mut self, branch_id: Option<&BranchId>, state: Option<RunState>) {
        if self.durable_state_seen {
            if self.durable_state_branch.as_ref() != branch_id {
                self.durable_state_corrupt = true;
                return;
            }
        } else {
            self.durable_state_seen = true;
            self.durable_state_branch = branch_id.cloned();
        }
        if let Some(state) = state {
            self.durable_state = Some(state);
        } else if self.durable_state.is_none() {
            self.durable_state = Some(RunState::Queued);
        }
    }
}

/// Live counterpart of startup effect reconciliation, scoped to one run.
///
/// Dispatcher close is attempted first, but its return value is not evidence
/// that every held dispatch reached a terminal journal record. Durable truth
/// is reduced here and missing outcomes are appended before any run terminal.
async fn reconcile_unknown_effects(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    mode: UnknownReconcile,
) -> Result<Option<RunState>, HaiderError> {
    let target = [UnknownReconcileTarget { run_id, branch_id }];
    let scan = scan_unknown_effects(store, &target)
        .await?
        .pop()
        .unwrap_or_default();
    append_unknown_effect_scan(store, device_id, run_id, branch_id, event_ids, mode, scan).await
}

async fn scan_unknown_effects(
    store: &HubStoreHandle,
    targets: &[UnknownReconcileTarget<'_>],
) -> Result<Vec<UnknownEffectScan>, HaiderError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let target_indices = targets
        .iter()
        .enumerate()
        .map(|(index, target)| (target.run_id.as_str(), (index, target.branch_id)))
        .collect::<HashMap<_, _>>();
    let mut scans = std::iter::repeat_with(UnknownEffectScan::default)
        .take(targets.len())
        .collect::<Vec<_>>();
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 512).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for mut envelope in page {
            let Some(run_id) = envelope.run_id.as_ref() else {
                continue;
            };
            let Some(&(index, expected_branch)) = target_indices.get(run_id.as_str()) else {
                continue;
            };
            let effect_scoped = envelope.branch_id.as_ref() == expected_branch;
            let payload_kind = envelope
                .payload
                .get("type")
                .and_then(serde_json::Value::as_str);
            let scan = &mut scans[index];
            if payload_kind == Some("user_message") {
                // The message body and attachments are irrelevant to durable
                // run-state reduction; avoid materializing them just to infer
                // the acceptance-time Queued default.
                scan.observe_durable_state(envelope.branch_id.as_ref(), None);
                continue;
            }
            if payload_kind == Some("run_state") {
                let Some(state) = envelope
                    .payload
                    .get_mut("state")
                    .map(serde_json::Value::take)
                else {
                    continue;
                };
                if let Ok(state) = serde_json::from_value::<RunState>(state) {
                    scan.observe_durable_state(envelope.branch_id.as_ref(), Some(state));
                }
                continue;
            }
            if !effect_scoped
                || !matches!(
                    payload_kind,
                    Some("effect" | "menu_opened" | "menu_answered" | "menu_closed")
                )
            {
                continue;
            }
            let payload =
                serde_json::from_value::<EventPayload>(envelope.payload).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "invalid effect payload in session {}, seq {}: {error}",
                            store.session_id(),
                            envelope.seq
                        ),
                        false,
                    )
                })?;
            match payload {
                EventPayload::Effect(EffectPhase::Intent(intent)) => {
                    scan.summaries.insert(intent.effect, intent.summary);
                }
                EventPayload::Effect(EffectPhase::Dispatched { effect }) => {
                    scan.dispatched.insert(effect);
                }
                EventPayload::Effect(EffectPhase::Outcome {
                    effect, outcome, ..
                }) => {
                    scan.outcomes
                        .insert(effect, matches!(outcome, EffectOutcome::Unknown));
                }
                EventPayload::MenuOpened(menu) => {
                    if let MenuKind::Recovery { effect, .. } = menu.kind {
                        scan.open_recovery.insert(menu.id, effect);
                    }
                }
                EventPayload::MenuAnswered(MenuAnswer { menu, .. })
                | EventPayload::MenuClosed { menu, .. } => {
                    scan.open_recovery.remove(&menu);
                }
                _ => {}
            }
        }
    }
    Ok(scans)
}

#[allow(clippy::too_many_arguments)]
async fn append_unknown_effect_scan(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    mode: UnknownReconcile,
    scan: UnknownEffectScan,
) -> Result<Option<RunState>, HaiderError> {
    let UnknownEffectScan {
        dispatched,
        summaries,
        outcomes,
        open_recovery,
        durable_state,
        durable_state_branch: _,
        durable_state_seen: _,
        durable_state_corrupt,
    } = scan;
    let mut durable_state = (!durable_state_corrupt).then_some(durable_state).flatten();
    let open_recovery_effects = open_recovery
        .values()
        .map(EffectId::as_str)
        .collect::<HashSet<_>>();
    let mut pending = dispatched
        .into_iter()
        .filter(|effect| {
            outcomes.get(effect).is_none_or(|unknown| *unknown)
                && !open_recovery_effects.contains(effect.as_str())
        })
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    if pending.is_empty() {
        return Ok(durable_state);
    }
    let evidence = if mode == UnknownReconcile::Park {
        effect_recovery_evidence(store, store.session_id(), Some(run_id), branch_id, &pending).await
    } else {
        HashMap::new()
    };
    let mut payloads = Vec::with_capacity(pending.len().saturating_mul(3));
    for effect in pending {
        if !outcomes.contains_key(&effect) {
            payloads.push(EventPayload::Effect(EffectPhase::Outcome {
                effect: effect.clone(),
                outcome: if mode == UnknownReconcile::Cancel {
                    EffectOutcome::Cancelled
                } else {
                    EffectOutcome::Unknown
                },
                freshness: None,
                workspace_mutation: None,
            }));
        }
        if mode == UnknownReconcile::Park {
            let mut menu = effect_recovery_menu(
                MenuId::new(format!("effect-recovery-{run_id}-{effect}")),
                effect.clone(),
                summaries
                    .get(&effect)
                    .map(String::as_str)
                    .unwrap_or("unknown dispatched effect"),
            );
            menu.body.push(
                evidence
                    .get(&effect)
                    .cloned()
                    .unwrap_or_else(|| "probe: evidence unavailable".into()),
            );
            payloads.push(EventPayload::MenuOpened(menu));
            payloads.push(EventPayload::RunState(RunState::EffectOutcomeUnknown));
            durable_state = Some(RunState::EffectOutcomeUnknown);
        }
    }
    if payloads.is_empty() {
        return Ok(durable_state);
    }
    append_payloads(store, device_id, run_id, branch_id, event_ids, payloads).await?;
    Ok(durable_state)
}

async fn durable_run_state(store: &HubStoreHandle, run_id: &RunId) -> Option<RunState> {
    durable_runs(store).await.ok().and_then(|runs| {
        runs.into_iter()
            .find_map(|(candidate, state, _, _, _)| (candidate == *run_id).then_some(state))
    })
}

async fn durable_queue_consumed(
    store: &HubStoreHandle,
    run_id: &RunId,
) -> Result<bool, HaiderError> {
    let mut cursor = 0;
    let mut consumed = false;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(consumed);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(EventPayload::QueueChanged(delta)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            match delta.change {
                QueueChange::Consumed { .. } => consumed = true,
                QueueChange::Enqueued { .. }
                | QueueChange::Removed { .. }
                | QueueChange::PromotedSteer { .. } => consumed = false,
                QueueChange::Unknown => {}
                _ => {}
            }
        }
    }
}

async fn durable_workspace_mutation(
    store: &HubStoreHandle,
    run_id: &RunId,
    effect_id: &EffectId,
) -> ToolResult<WorkspaceMutation> {
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256)
            .await
            .map_err(|error| ToolError::Runtime {
                message: error.message,
            })?;
        if page.is_empty() {
            return Err(ToolError::Runtime {
                message: format!(
                    "workspace mutation for effect {effect_id} was not durable after completion"
                ),
            });
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id) {
                continue;
            }
            let Ok(EventPayload::Effect(EffectPhase::Outcome {
                effect,
                workspace_mutation: Some(mutation),
                ..
            })) = serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            if &effect == effect_id {
                if mutation.workspace_revision.is_none() || mutation.subject_digest.is_none() {
                    return Err(ToolError::Runtime {
                        message: format!(
                            "workspace mutation for effect {effect_id} lacks daemon provenance"
                        ),
                    });
                }
                return Ok(mutation);
            }
        }
    }
}

/// Parks a recoverable checkpoint/delegated child across a GRACEFUL drain:
/// stop the harness actor and close the broker WITHOUT cancelling, appending
/// a terminal, or reconciling effects. The next generation reconstructs a
/// menu/child wait directly; an interrupted delegated child stays parked for
/// its recovered parent's durable W6c supervision. The supervisor's exit
/// unregisters the lease without changing run truth.
async fn park_request_input_checkpoint(mut turn: ActiveTurn) {
    // Abort the harness actor FIRST — `HarnessHandle::stop` would drive the
    // core cancellation ladder (MenuClosed, item cancellation, Cancelling),
    // which is exactly what parking must not do. Aborting at the actor's
    // menu-watch await point appends nothing, the same silence a crash
    // leaves behind.
    if let Some(actor) = turn.actor.take() {
        actor.abort();
        let _ = actor.await;
    }
    if let Some(dispatcher) = turn.dispatcher.take() {
        // Quiet broker close AFTER the actor is gone: a parked run has no
        // in-flight effect dispatch (the provider request completed and the
        // tool is awaiting input), so closing releases resources without
        // producing outcomes.
        let _ = dispatcher.close().await;
    }
    // Dropping the turn (harness handle + outcome future) appends nothing.
}

async fn cancel_durable_queued_turns(
    queue: &mut VecDeque<PendingTurn>,
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    active_run: Option<&RunId>,
) -> Option<RunId> {
    let mut last = None;
    for (run_id, state, _, branch_id, _) in durable_runs(store).await.unwrap_or_default() {
        if active_run == Some(&run_id) || state != RunState::Queued {
            continue;
        }
        if let Err(error) = append_run_state(
            store,
            device_id,
            &run_id,
            branch_id.as_ref(),
            event_ids,
            RunState::Cancelled,
        )
        .await
        {
            tracing::warn!(%run_id, ?error, "queued turn could not be terminalized during drain");
        }
        last = Some(run_id);
    }
    while let Some(mut pending) = queue.pop_front() {
        if let Some(ready) = pending.recovery_ready.take() {
            let _ = ready.send(Err(HaiderError::new(
                ErrorCode::Busy,
                "daemon drained before recovered checkpoint could start",
                true,
            )));
        }
    }
    last
}

struct DurableCompactionReceipt {
    run_id: RunId,
    accepted_seq: u64,
    worker_generation: u64,
    intent: CompactionIntent,
    branch_id: Option<BranchId>,
    committed: bool,
}

async fn find_compaction_receipt(
    store: &HubStoreHandle,
    command_id: &str,
    branch_id: Option<&BranchId>,
) -> Result<Option<DurableCompactionReceipt>, HaiderError> {
    let operation_id = format!("manual-{command_id}");
    let node_id = format!("compaction-node-{operation_id}");
    let mut receipt = None;
    let mut committed = false;
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.branch_id.as_ref() != branch_id {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { kind, data },
                    ..
                }) if kind == COMPACTION_INTENT_EXTENSION_KIND => {
                    let intent =
                        serde_json::from_value::<CompactionIntent>(data).map_err(|error| {
                            HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                format!("manual compaction intent is invalid: {error}"),
                                false,
                            )
                        })?;
                    if intent.operation_id != operation_id {
                        continue;
                    }
                    let run_id = envelope.run_id.ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            "manual compaction intent has no run id",
                            false,
                        )
                    })?;
                    receipt = Some((
                        run_id,
                        envelope.seq,
                        envelope.worker_generation,
                        envelope.branch_id.clone(),
                        intent,
                    ));
                }
                EventPayload::NodeCommitted(node) if node.node.as_str() == node_id => {
                    committed = true;
                }
                _ => {}
            }
        }
    }
    Ok(receipt.map(
        |(run_id, accepted_seq, worker_generation, branch_id, intent)| DurableCompactionReceipt {
            run_id,
            accepted_seq,
            worker_generation,
            intent,
            branch_id,
            committed,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
async fn perform_manual_compaction(
    dependencies: &WorkerDependencies,
    metadata: &SessionMetadataV1,
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: Arc<EventIdGenerator>,
    command_id: String,
    worker_generation: u64,
    branch_id: Option<BranchId>,
) -> Result<AcceptedCompaction, HaiderError> {
    let request_json = serde_json::to_string(&serde_json::json!({
        "session_id": lease.session_id(),
        "worker_generation": worker_generation,
        "branch_id": branch_id,
    }))
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("session-compact request could not serialize: {error}"),
            false,
        )
    })?;
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    // The global command receipt is claimed before intent. Its committed
    // replay wins over generation fencing, exactly like turn submission.
    match lease
        .claim_context_compaction_receipt(command_id.clone(), request_digest, request_json)
        .await?
    {
        ContextCompactionClaim::Committed(response) => {
            return Ok(AcceptedCompaction {
                run_id: response.run_id,
                accepted_seq: response.accepted_seq,
                worker_generation: response.worker_generation,
                branch_id: response.branch_id,
            });
        }
        ContextCompactionClaim::Fresh | ContextCompactionClaim::ResumePending => {}
    }

    let existing = find_compaction_receipt(lease, &command_id, branch_id.as_ref()).await?;
    if let Some(receipt) = existing.as_ref()
        && receipt.committed
    {
        let accepted = AcceptedCompaction {
            run_id: receipt.run_id.clone(),
            accepted_seq: receipt.accepted_seq,
            worker_generation: receipt.worker_generation,
            branch_id: receipt.branch_id.clone(),
        };
        lease
            .finalize_context_compaction_receipt(
                command_id,
                ContextCompactionReceiptResponse {
                    session_id: lease.session_id().clone(),
                    run_id: accepted.run_id.clone(),
                    accepted_seq: accepted.accepted_seq,
                    worker_generation: accepted.worker_generation,
                    branch_id: accepted.branch_id.clone(),
                },
            )
            .await?;
        return Ok(accepted);
    }
    if worker_generation != lease.worker_generation() {
        return Err(HaiderError::new(
            ErrorCode::SingleWriterViolation,
            format!(
                "manual compaction generation {worker_generation} is stale; current generation is {}",
                lease.worker_generation()
            ),
            false,
        ));
    }
    if existing.is_none()
        && durable_runs(lease)
            .await?
            .iter()
            .any(|(_, state, _, _, _)| !state.is_terminal())
    {
        return Err(HaiderError::new(
            ErrorCode::Busy,
            "manual context compaction is idle-only",
            true,
        ));
    }

    let delegation = dependencies.delegation.clone().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "worker delegation coordinator is not installed",
            false,
        )
    })?;
    let delegation_record = delegation.record_for_session(lease.session_id()).await?;
    let agent_id = delegation_record
        .as_ref()
        .map(|record| record.agent_id.clone());
    let grant = delegation_record
        .as_ref()
        .map(|record| &record.manifest.grant);
    if let Some(grant) = grant {
        validate_grant(grant)?;
    }
    let mut web_degrade = lease.hub().web_degrade(lease.session_id());
    if grant.is_some_and(|grant| {
        !effect_within_grant(
            grant,
            &EffectClass::Network {
                host: String::new(),
            },
        )
    }) {
        web_degrade.anthropic_web_tools = true;
    }
    let resolved = dependencies
        .provider_factory
        .resolve_for_turn_with_web(metadata, web_degrade)
        .await?;
    if resolved.provider_name != metadata.provider {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "provider factory returned a different provider than the session",
            false,
        ));
    }
    dependencies
        .provider_factory
        .reconcile_cache_scope(lease.session_id(), &resolved.provider_name)
        .await;
    let instructions = project_instructions::load(&metadata.cwd).await;
    let instruction_entries = instructions
        .as_ref()
        .map_or_else(Vec::new, LoadedProjectInstructions::prompt_entries);
    let handoff_dir = delegation
        .handoff_dir_for_child_session(lease.session_id(), &metadata.cwd)
        .await?;
    let mobile_use_active = durable_session_tool_state(lease, lease.session_id())
        .await?
        .mobile_use_active;
    let post_compaction_tools = advertised_tool_definitions_for_mobile_state(
        &dependencies.tool_factory,
        grant,
        &resolved.provider_name,
        web_degrade,
        mobile_use_active,
    );
    let post_compaction_system_prompt = {
        let mut prompt = SystemPromptBuilder::build_with_handoff(
            metadata,
            &instruction_entries,
            handoff_dir.as_deref(),
        );
        // Same instruct-pipe manual as the live turn, so the cached system
        // prefix is identical across normal and post-compaction requests.
        prompt.push_str(&tool_manual(&post_compaction_tools));
        prompt
    };
    let auth_scope = credential_surface_name(resolved.provider.credential_surface()).to_owned();
    let usage_account = resolved
        .account_alias
        .as_deref()
        .map(haider_protocol::ids::CredentialAlias::new);
    let reasoning_settings = serde_json::to_string(&serde_json::json!({
        "effort": metadata.effort,
        "fast": metadata.fast,
    }))
    .unwrap_or_default();
    let mut usage_scope = usage_scope_for(
        &resolved.provider_name,
        &resolved.model,
        usage_account.clone(),
        &auth_scope,
        &reasoning_settings,
        &Some(post_compaction_system_prompt.clone()),
        &post_compaction_tools,
    );
    stamp_usage_lane_dimensions(&mut usage_scope, resolved.provider.usage_lane_dimensions());
    let (mut messages, latest_compaction_summary_end) =
        PromptHistoryCompiler::compile_idle_with_artifacts_and_boundary(
            lease,
            lease,
            lease.session_id(),
            branch_id.as_ref(),
            agent_id.as_ref(),
        )
        .await?;
    prepare_compaction_messages(lease, &mut messages).await?;
    let (run_id, accepted_seq, intent) = if let Some(existing) = existing {
        (existing.run_id, existing.accepted_seq, existing.intent)
    } else {
        let run_id = RunId::new(format!("manual-compact-{command_id}"));
        let intent = PromptHistoryCompiler::plan_idle_compaction(
            lease,
            lease.session_id(),
            branch_id.as_ref(),
            agent_id.as_ref(),
            format!("manual-{command_id}"),
        )
        .await?;
        let intent_item_id = ItemId::new(format!("compaction-intent-{}", intent.operation_id));
        let intent_item = TurnItem::Extension {
            kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
            data: serde_json::to_value(&intent).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("manual compaction intent could not serialize: {error}"),
                    false,
                )
            })?,
        };
        let mut envelopes = vec![
            supervisor_envelope(
                lease,
                device_id,
                branch_id.clone(),
                Some(run_id.clone()),
                event_ids.next(),
                EventPayload::Item(ItemEvent::Started {
                    item_id: intent_item_id.clone(),
                    item: intent_item.clone(),
                }),
            )?,
            supervisor_envelope(
                lease,
                device_id,
                branch_id.clone(),
                Some(run_id.clone()),
                event_ids.next(),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: intent_item_id,
                    item: intent_item,
                }),
            )?,
            supervisor_envelope(
                lease,
                device_id,
                branch_id.clone(),
                Some(run_id.clone()),
                event_ids.next(),
                EventPayload::RunState(RunState::Compacting),
            )?,
        ];
        for envelope in &mut envelopes {
            envelope.agent_id = agent_id.clone();
        }
        let range = StoreHandle::append(lease, &mut envelopes).await?;
        (run_id, range.first_seq.saturating_add(1), intent)
    };
    let cache_expected_later_reads = u32::from(!post_compaction_tools.is_empty()) * 2;
    let compactor = DaemonContextCompactor {
        store: lease.clone(),
        provider: resolved.provider,
        model: resolved.model,
        max_tokens: metadata.max_tokens,
        context_window: resolved.context_window,
        reserved_output_tokens: metadata.max_tokens,
        post_compaction_system_prompt: Some(post_compaction_system_prompt),
        post_compaction_tools,
        reasoning_settings,
        cache_expected_later_reads,
        cache_reuse_gap_ms: None,
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
        agent_id,
        branch_id: branch_id.clone(),
        usage_scope,
        usage_account,
    };
    if let Err(error) = compactor
        .compact(
            &run_id,
            &intent,
            messages,
            Vec::new(),
            latest_compaction_summary_end,
        )
        .await
    {
        append_failure(
            lease,
            device_id,
            &run_id,
            branch_id.as_ref(),
            &event_ids,
            error.clone(),
        )
        .await?;
        return Err(error);
    }
    append_session_idle(lease, device_id, &event_ids, false).await?;
    let accepted = AcceptedCompaction {
        run_id,
        accepted_seq,
        worker_generation,
        branch_id: branch_id.clone(),
    };
    lease
        .finalize_context_compaction_receipt(
            command_id,
            ContextCompactionReceiptResponse {
                session_id: lease.session_id().clone(),
                run_id: accepted.run_id.clone(),
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                branch_id: accepted.branch_id.clone(),
            },
        )
        .await?;
    Ok(accepted)
}

#[allow(clippy::too_many_arguments)]
async fn perform_shell_exec(
    metadata: &SessionMetadataV1,
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: Arc<EventIdGenerator>,
    diagnostics: Option<Arc<EffectDiagnostics>>,
    pending: PendingShellExec,
    cancellation_wakes: &mut tokio::sync::watch::Receiver<u64>,
    drain_wakes: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), HaiderError> {
    let run_id = pending.accepted.run_id.clone();
    let state = durable_run_state(lease, &run_id).await;
    if state.as_ref().is_some_and(RunState::is_terminal) {
        // A same-command receipt replay may hand the accepted job to the
        // supervisor again. Terminal durable truth makes that a no-op.
        return Ok(());
    }
    if state == Some(RunState::Cancelling) {
        return cancel_shell_exec(
            lease,
            device_id,
            &event_ids,
            &run_id,
            pending.branch_id.as_ref(),
            pending.agent_id.as_ref(),
        )
        .await;
    }
    if state != Some(RunState::RunningTool) {
        return Err(HaiderError::new(
            ErrorCode::RunNotActive,
            format!("direct shell run {run_id} is not durably running"),
            false,
        ));
    }
    if *drain_wakes.borrow() {
        begin_shell_cancellation(
            lease,
            device_id,
            &event_ids,
            &run_id,
            pending.branch_id.as_ref(),
            pending.agent_id.as_ref(),
        )
        .await?;
        return cancel_shell_exec(
            lease,
            device_id,
            &event_ids,
            &run_id,
            pending.branch_id.as_ref(),
            pending.agent_id.as_ref(),
        )
        .await;
    }

    let shell = match ShellSession::new(&metadata.cwd, Vec::new()) {
        Ok(shell) => shell,
        Err(error) => {
            return fail_shell_exec(
                lease,
                device_id,
                &event_ids,
                &run_id,
                pending.branch_id.as_ref(),
                pending.agent_id.as_ref(),
                tool_error(error),
            )
            .await;
        }
    };
    let operation = match shell.prepare_user_process(
        pending.command_id.clone(),
        pending.command.clone(),
        pending.cwd.as_deref().map(Path::new),
    ) {
        Ok(operation) => operation,
        Err(error) => {
            return fail_shell_exec(
                lease,
                device_id,
                &event_ids,
                &run_id,
                pending.branch_id.as_ref(),
                pending.agent_id.as_ref(),
                tool_error(error),
            )
            .await;
        }
    };
    let journal = HubJournalSink {
        store: lease.clone(),
        run_id: run_id.clone(),
        branch_id: pending.branch_id.clone(),
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
        diagnostics,
        workspace_root_digest: EffectDiagnostics::workspace_digest(&metadata.cwd),
        active_tool_name: Arc::new(StdMutex::new(Some("shell_exec".into()))),
        intent_digests: HashMap::new(),
        pending_breadcrumbs: HashMap::new(),
    };
    let mut broker = match EffectBroker::new(
        Box::new(journal),
        &metadata.cwd,
        lease.session_id().clone(),
        lease.worker_generation(),
    ) {
        Ok(broker) => broker,
        Err(error) => {
            return fail_shell_exec(
                lease,
                device_id,
                &event_ids,
                &run_id,
                pending.branch_id.as_ref(),
                pending.agent_id.as_ref(),
                tool_error(error),
            )
            .await;
        }
    };
    let output_context = HubCommandOutputContext {
        store: lease.clone(),
        branch_id: pending.branch_id.clone(),
        agent_id: pending.agent_id.clone(),
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
    };
    let output = output_context.sink(
        run_id.clone(),
        pending.accepted.item_id.clone(),
        pending.command_id,
        PromptRender::Verbatim,
    );
    let execution = match broker
        .process_exec_user(
            &operation,
            HubArtifactStore {
                store: lease.clone(),
            },
            output,
            ProcessBounds::default(),
        )
        .await
    {
        Ok(execution) => execution,
        Err(error) => {
            let error = tool_error(error);
            let _ = broker.close().await;
            return fail_shell_exec(
                lease,
                device_id,
                &event_ids,
                &run_id,
                pending.branch_id.as_ref(),
                pending.agent_id.as_ref(),
                error,
            )
            .await;
        }
    };
    // Wait stays owned by the supervisor task so forced supervisor teardown
    // drops `ProcessExecution` and cannot detach a child waiter. The cloned
    // capability can request the same TERM → grace → KILL path while this
    // future continues to own and observe the final process result.
    let process_cancel = execution.cancel_handle();
    let wait = execution.wait();
    tokio::pin!(wait);
    let mut cancellation_channel_open = true;
    let mut drain_channel_open = true;
    let result = loop {
        tokio::select! {
            biased;
            changed = drain_wakes.changed(), if drain_channel_open => {
                if changed.is_err() {
                    drain_channel_open = false;
                    continue;
                }
                let draining = *drain_wakes.borrow_and_update();
                if draining {
                    begin_shell_cancellation(
                        lease,
                        device_id,
                        &event_ids,
                        &run_id,
                        pending.branch_id.as_ref(),
                        pending.agent_id.as_ref(),
                    )
                    .await?;
                    process_cancel.cancel();
                    let _ = wait.await;
                    if let Err(error) = broker.cancel().await {
                        tracing::warn!(
                            %run_id,
                            ?error,
                            "direct shell broker close reported an error during daemon drain"
                        );
                    }
                    return cancel_shell_exec(
                        lease,
                        device_id,
                        &event_ids,
                        &run_id,
                        pending.branch_id.as_ref(),
                        pending.agent_id.as_ref(),
                    )
                    .await;
                }
            }
            changed = cancellation_wakes.changed(), if cancellation_channel_open => {
                if changed.is_err() {
                    cancellation_channel_open = false;
                    continue;
                }
                if durable_run_state(lease, &run_id).await == Some(RunState::Cancelling) {
                    process_cancel.cancel();
                    let _ = wait.await;
                    if let Err(error) = broker.cancel().await {
                        tracing::warn!(
                            %run_id,
                            ?error,
                            "direct shell broker close reported an error during cancellation"
                        );
                    }
                    return cancel_shell_exec(
                        lease,
                        device_id,
                        &event_ids,
                        &run_id,
                        pending.branch_id.as_ref(),
                        pending.agent_id.as_ref(),
                    )
                    .await;
                }
            }
            result = &mut wait => {
                break match result {
                    Ok(result) => result,
                    Err(error) => {
                        let error = tool_error(error);
                        let _ = broker.close().await;
                        return fail_shell_exec(
                            lease,
                            device_id,
                            &event_ids,
                            &run_id,
                            pending.branch_id.as_ref(),
                            pending.agent_id.as_ref(),
                            error,
                        )
                        .await;
                    }
                };
            }
        }
    };
    if let Err(error) = broker.close().await {
        return fail_shell_exec(
            lease,
            device_id,
            &event_ids,
            &run_id,
            pending.branch_id.as_ref(),
            pending.agent_id.as_ref(),
            HaiderError::new(
                ErrorCode::EffectUnknownOutcome,
                format!("direct shell broker close reported unfinished work: {error}"),
                false,
            ),
        )
        .await;
    }
    if durable_run_state(lease, &run_id).await == Some(RunState::Cancelling) {
        return cancel_shell_exec(
            lease,
            device_id,
            &event_ids,
            &run_id,
            pending.branch_id.as_ref(),
            pending.agent_id.as_ref(),
        )
        .await;
    }
    output_context
        .record_process_signal(&run_id, &result)
        .await?;
    let completed = append_shell_completion(
        lease,
        device_id,
        &run_id,
        &event_ids,
        pending.branch_id.as_ref(),
        pending.agent_id.as_ref(),
        EventPayload::Item(ItemEvent::Completed {
            item_id: pending.accepted.item_id,
            item: result.completed_item(pending.command),
        }),
    )
    .await;
    if let Err(error) = completed {
        if durable_run_state(lease, &run_id).await == Some(RunState::Cancelling) {
            return cancel_shell_exec(
                lease,
                device_id,
                &event_ids,
                &run_id,
                pending.branch_id.as_ref(),
                pending.agent_id.as_ref(),
            )
            .await;
        }
        return Err(error);
    }
    append_session_idle(lease, device_id, &event_ids, false).await
}

async fn begin_shell_cancellation(
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
) -> Result<(), HaiderError> {
    match durable_run_state(lease, run_id).await {
        Some(RunState::RunningTool) => {
            append_shell_payloads(
                lease,
                device_id,
                run_id,
                branch_id,
                agent_id,
                event_ids,
                vec![EventPayload::RunState(RunState::Cancelling)],
            )
            .await
        }
        Some(RunState::Cancelling) | Some(RunState::Cancelled) => Ok(()),
        Some(state) if state.is_terminal() => Ok(()),
        state => Err(HaiderError::new(
            ErrorCode::RunNotActive,
            format!("direct shell run {run_id} cannot begin cancellation from {state:?}"),
            false,
        )),
    }
}

async fn cancel_shell_exec(
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
) -> Result<(), HaiderError> {
    let _ = reconcile_unknown_effects(
        lease,
        device_id,
        run_id,
        branch_id,
        event_ids,
        UnknownReconcile::Cancel,
    )
    .await?;
    let mut payloads = cancelled_resumption_payloads(lease, lease.session_id(), run_id).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    append_shell_payloads(
        lease, device_id, run_id, branch_id, agent_id, event_ids, payloads,
    )
    .await?;
    append_session_idle(lease, device_id, event_ids, true).await
}

async fn fail_shell_exec(
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    error: HaiderError,
) -> Result<(), HaiderError> {
    if durable_run_state(lease, run_id).await == Some(RunState::Cancelling) {
        return cancel_shell_exec(lease, device_id, event_ids, run_id, branch_id, agent_id).await;
    }
    let _ = reconcile_unknown_effects(
        lease,
        device_id,
        run_id,
        branch_id,
        event_ids,
        UnknownReconcile::EvidenceOnly,
    )
    .await?;
    let mut payloads =
        failed_resumption_payloads(lease, lease.session_id(), run_id, &error).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    if let Err(append_error) = append_shell_payloads(
        lease, device_id, run_id, branch_id, agent_id, event_ids, payloads,
    )
    .await
    {
        if durable_run_state(lease, run_id).await == Some(RunState::Cancelling) {
            return cancel_shell_exec(lease, device_id, event_ids, run_id, branch_id, agent_id)
                .await;
        }
        return Err(append_error);
    }
    append_session_idle(lease, device_id, event_ids, false).await
}

/// The provider-opaque tag a session provider's wire accepts (G3/LT3).
/// Everything outside the three native families speaks the chat-completions
/// dialect, whose wire accepts the `openai-compatible` tag.
fn accepted_opaque_provider(provider_name: &str) -> &'static str {
    use haider_provider::{
        GEMINI_PROVIDER_NAME, OPENAI_COMPATIBLE_PROVIDER_NAME, OPENAI_OAUTH_PROVIDER_NAME,
        OPENAI_PROVIDER_NAME,
    };
    match provider_name {
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME => ANTHROPIC_PROVIDER_NAME,
        OPENAI_PROVIDER_NAME | OPENAI_OAUTH_PROVIDER_NAME => OPENAI_PROVIDER_NAME,
        GEMINI_PROVIDER_NAME => GEMINI_PROVIDER_NAME,
        _ => OPENAI_COMPATIBLE_PROVIDER_NAME,
    }
}

/// Drops provider-opaque blocks minted by a DIFFERENT provider family from
/// the compiled prompt (G3/LT3), and with them any message left empty — an
/// empty assistant content array is itself a wire error. Same-family facts
/// pass through untouched: this is a strip, never a rewrite.
fn strip_foreign_provider_opaque(messages: &mut Vec<Message>, provider_name: &str) {
    let accepted = accepted_opaque_provider(provider_name);
    for message in messages.iter_mut() {
        message.blocks.retain(|block| {
            !matches!(
                block,
                haider_protocol::provider::Block::ProviderOpaque { provider, .. }
                    if provider != accepted
            )
        });
    }
    messages.retain(|message| !message.blocks.is_empty());
}

/// Applies the provider-family opaque strip while remapping every compiler
/// boundary through messages that became empty. The strip remains a pure
/// removal; signed/native blocks that survive are never rewritten.
fn strip_foreign_provider_opaque_projection(
    projection: &mut CompiledPromptProjection,
    provider_name: &str,
) {
    let stable_before = projection.stable_history_end;
    let current_before = projection.current_user_start;
    let summary_before = projection.latest_compaction_summary_end;
    let accepted = accepted_opaque_provider(provider_name);
    let mut boundary_map = Vec::with_capacity(projection.messages.len().saturating_add(1));
    boundary_map.push(0usize);
    let mut retained = 0usize;
    for message in &projection.messages {
        let survives = message.blocks.iter().any(|block| {
            !matches!(
                block,
                haider_protocol::provider::Block::ProviderOpaque { provider, .. }
                    if provider != accepted
            )
        });
        retained = retained.saturating_add(usize::from(survives));
        boundary_map.push(retained);
    }
    strip_foreign_provider_opaque(&mut projection.messages, provider_name);
    let remap = |boundary: usize| {
        boundary_map
            .get(boundary.min(boundary_map.len().saturating_sub(1)))
            .copied()
            .unwrap_or(projection.messages.len())
    };
    projection.stable_history_end = remap(stable_before);
    projection.current_user_start = remap(current_before);
    projection.latest_compaction_summary_end = summary_before.map(remap);
}

/// Re-reads THIS session's metadata for one logical turn (F1). The store is
/// the one truth for the current model selection: a committed
/// `session.select_model` between turns is picked up here, which is what
/// makes the receipt's promise — "the next turn resolves the new pair" —
/// structural rather than aspirational. `None` (row vanished mid-life) and
/// read failures fail the turn honestly; the spawn-time snapshot is never a
/// silent fallback.
async fn fresh_turn_metadata(lease: &HubStoreHandle) -> Result<SessionMetadataV1, HaiderError> {
    lease.session_metadata().await?.ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            "session metadata disappeared before turn start",
            false,
        )
    })
}

/// Assembles and starts one accepted turn: provider resolution (R6 pinning —
/// this is the once-per-logical-turn call), committed-history compilation
/// (R4), tool dispatcher creation, harness registration under the lease, and
/// submission.
///
/// Checkpoint resumption order is deliberate: the recovered harness
/// registers FIRST, then the journal is scanned for an already-committed
/// answer. An answer committed before registration is found by the scan; one
/// committed after the scan is delivered by the hub's registered-harness
/// wake; one committed between registration and the scan is sent TWICE (hub
/// wake at commit, then the scan's apply). The duplicate is safe because
/// both sends land in the harness's latest-value committed-menu watch before
/// the checkpoint turn's waiter performs its first read, which collapses
/// them into one observation — missing the answer is the failure mode this
/// ordering exists to prevent.
async fn start_turn(
    dependencies: &WorkerDependencies,
    metadata: &SessionMetadataV1,
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: Arc<EventIdGenerator>,
    pending: PendingTurn,
) -> Result<ActiveTurn, HaiderError> {
    let PendingTurn {
        accepted,
        prompt_run_id,
        checkpoint,
        partial_stream,
        child_wait,
        mut committed_answer,
        recovery_ready: _,
        recovering: _,
    } = pending;
    // W-B (decision 8): the session's web-capability degrades ride into
    // pair resolution (native declarations) AND the tool pack below — ONE
    // per-turn derivation from the resolved pair.
    let mut web_degrade = lease.hub().web_degrade(lease.session_id());
    let delegation = dependencies.delegation.clone().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "worker delegation coordinator is not installed",
            false,
        )
    })?;
    let delegation_record = delegation.record_for_session(lease.session_id()).await?;
    let agent_id = delegation_record
        .as_ref()
        .map(|record| record.agent_id.clone());
    let grant = delegation_record
        .as_ref()
        .map(|record| record.manifest.grant.clone());
    if let Some(grant) = grant.as_ref() {
        validate_grant(grant)?;
        if !effect_within_grant(
            grant,
            &EffectClass::Network {
                host: String::new(),
            },
        ) {
            web_degrade.anthropic_web_tools = true;
        }
    }
    // B3 (round 3) — the typed child's exec scope is MANIFEST truth, frozen
    // at spawn: registry edits never widen a running child's executable
    // set. Older typed manifests (pre-freeze) fall back to the daemon-truth
    // task prefix under deny-all-when-unresolvable.
    let cli_scope = match delegation_record.as_ref() {
        Some(record) if record.manifest.cli_scope.is_some() => record.manifest.cli_scope.clone(),
        Some(record) => match loom_task_type_id(&record.task) {
            Some(type_id) => Some(
                lease
                    .hub()
                    .loom_agent_type(&type_id)
                    .await?
                    .map(|typed| typed.clis)
                    .unwrap_or_default(),
            ),
            None => None,
        },
        None => None,
    };
    let resolved = dependencies
        .provider_factory
        .resolve_for_turn_with_web(metadata, web_degrade)
        .await?;
    if resolved.provider_name != metadata.provider {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "provider factory returned a different provider than the session",
            false,
        ));
    }
    // Explicit Gemini resources are session-owned rather than provider-
    // instance-owned. Reconcile on every ordinary turn after resolving the
    // selected pair so switching away deletes the old paid resource before
    // any request is sent on the new provider. Implementations for providers
    // without explicit resources retain the additive no-op default.
    dependencies
        .provider_factory
        .reconcile_cache_scope(lease.session_id(), &resolved.provider_name)
        .await;
    // G1 (L5): a delegation-owned session is a child — its tool pack below
    // excludes the root-only planning surface.
    let handoff_dir = delegation
        .handoff_dir_for_child_session(lease.session_id(), &metadata.cwd)
        .await?;
    let instructions = project_instructions::load(&metadata.cwd).await;
    journal_project_instructions_if_changed(
        lease,
        device_id,
        &accepted.run_id,
        accepted.branch_id.as_ref(),
        agent_id.as_ref(),
        &event_ids,
        instructions.as_ref(),
    )
    .await?;
    let prewarm = (std::env::var_os("HAIDER_PROVIDER_PREWARM").as_deref()
        == Some(std::ffi::OsStr::new("1")))
    .then(|| {
        let provider = Arc::clone(&resolved.provider);
        tokio::spawn(async move {
            provider.prewarm().await;
        })
    });
    let prompt_compile_started = Instant::now();
    let prompt_run_id = prompt_run_id.as_ref().unwrap_or(&accepted.run_id);
    let mut compiled = lease
        .compile_prompt_projection(
            accepted.branch_id.as_ref(),
            agent_id.as_ref(),
            prompt_run_id,
        )
        .await?;
    // G3 (LT3): provider-opaque continuation facts are only valid on the
    // wire family that minted them — every adapter REJECTS a foreign tag.
    // After a cross-provider model switch the compiled history still carries
    // the old family's facts (openai encrypted reasoning, gemini signed
    // parts, anthropic thinking blocks), so they are stripped here, before
    // the request, instead of failing every turn on the new pair.
    strip_foreign_provider_opaque_projection(&mut compiled, &resolved.provider_name);
    let compiled_stable_history_end = compiled.stable_history_end;
    let compiled_current_user_start = compiled.current_user_start;
    let compiled_compaction_summary_end = compiled.latest_compaction_summary_end;
    let mut messages = compiled.messages;
    let provider_capabilities = resolved.provider.capabilities().await;
    if let Some(prewarm) = prewarm {
        let _ = prewarm.await;
    }
    if messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { .. }
                )
            )
        })
    }) && provider_capabilities.vision == FeatureResolve::Unsupported
    {
        return Err(HaiderError::new(
            ErrorCode::VisionUnsupported,
            format!(
                "provider `{}` does not support image attachments",
                resolved.provider_name
            ),
            false,
        ));
    }
    tracing::trace!(
        target: "haider.worker",
        session_id = %lease.session_id(),
        run_id = %accepted.run_id,
        prompt_messages = messages.len(),
        compile_micros = prompt_compile_started.elapsed().as_micros(),
        "prompt history compiled"
    );
    let attachments = resolve_prompt_attachments(
        lease,
        &mut messages,
        provider_capabilities.vision,
        provider_capabilities.pdf_documents,
    )
    .await?;
    // Dynamic graph state is deliberately outside durable prompt history and
    // the stable system/tool prefix. Root sessions receive one bounded,
    // turn-scoped user-role tail; delegated children receive no parent graph.
    let workflow_child = grant
        .as_ref()
        .is_some_and(|grant| grant.tools.iter().any(|tool| tool == "graph_evidence"));
    let graph_status = if agent_id.is_none() || workflow_child {
        lease
            .hub()
            .graph_status(lease.session_id())
            .await
            .map_err(hub_error)?
    } else {
        None
    };
    let graph_brief = graph_status
        .as_ref()
        .and_then(|status| status.graph_brief());
    // C1: when the pinned template is a REGISTERED LOOM WORKFLOW, ride its
    // typed-node manual beside the graph brief so the model runs each node
    // through the node's agent type (C2's typed spawn).
    let loom_tail = match graph_status.as_ref() {
        Some(status) => lease
            .hub()
            .loom_workflow(&status.template)
            .await?
            // Verify-fix C1 / review round 2: the PINNED instance is
            // immutable; the registry is not. The pin persisted
            // `graph_template_digest(template)`, so the tail joins on that
            // SAME key — the registry stamps `template.version = rev`, which
            // makes the template digest a faithful proxy for the whole
            // workflow identity. A re-registered rev mismatches and the tail
            // stays silent rather than teach nodes the pin does not enforce.
            .filter(|workflow| {
                haider_protocol::graph::graph_template_digest(&workflow.template) == status.digest
            })
            .map(|workflow| loom_run_tail(&workflow)),
        None => None,
    };
    // E1: the registry inventory rides the SAME volatile tail — the model
    // learns what specialists/workflows exist without a cache-epoch cost
    // (registrations move tail bytes only, never history).
    let (loom_inventory, agent_type_identity) = {
        let (types, workflows) = lease.hub().loom_registry().await?;
        // W-flow inline identity: the session's BOUND agent type performs
        // inside this session's context — its job rides the same volatile
        // tail, so binding or clearing never moves the cache epoch. A
        // binding whose type has left the registry stays silent rather
        // than teach a job the registry no longer holds.
        let identity = metadata.agent_type.as_deref().and_then(|bound| {
            types
                .iter()
                .find(|record| record.id == bound)
                .map(agent_type_identity_line)
        });
        (loom_inventory_line(&types, &workflows), identity)
    };
    let graph_brief = {
        let parts: Vec<String> = [graph_brief, loom_tail, agent_type_identity, loom_inventory]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    };
    let mobile_use_active = durable_session_tool_state(lease, lease.session_id())
        .await?
        .mobile_use_active;
    let dispatcher = dependencies
        .tool_factory
        .create(WorkerToolContext {
            metadata: metadata.clone(),
            store: lease.clone(),
            run_id: accepted.run_id.clone(),
            branch_id: accepted.branch_id.clone(),
            device_id: device_id.clone(),
            event_ids: Arc::clone(&event_ids),
            delegation,
            tasks: crate::tasks::TaskFacade::new(lease.hub().clone()),
            agent_id: agent_id.clone(),
            grant: grant.clone(),
            mobile_use_active,
            cli_scope,
            web_search: dependencies.web_search.clone(),
            diagnostics: dependencies.diagnostics.clone(),
        })
        .await?;
    let mut config = HarnessConfig::for_session(
        lease.session_id().clone(),
        device_id.clone(),
        0,
        lease.worker_generation(),
    )
    .with_event_ids(Arc::clone(&event_ids));
    let provider_request_state = provider_derived_request_state(
        &resolved.provider_name,
        &provider_capabilities,
        web_degrade,
    );
    config.cached_input_is_subset = cached_input_is_subset_for_provider(&resolved.provider_name);
    config.context_compaction_v1 = true;
    config.compaction_guard_v1 = true;
    config.model = resolved.model;
    config.context_window = resolved.context_window;
    config.agent_id = agent_id;
    config.branch_id = accepted.branch_id.clone();
    config.max_tokens = metadata.max_tokens;
    config.reserved_output_tokens = metadata.max_tokens;
    if let Some(window) = config.context_window
        && config.reserved_output_tokens >= window
    {
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!(
                "reserved output budget {} must be smaller than model context window {}",
                config.reserved_output_tokens, window
            ),
            false,
        ));
    }
    let instruction_entries = instructions
        .as_ref()
        .map_or_else(Vec::new, LoadedProjectInstructions::prompt_entries);
    config.system_prompt = Some(SystemPromptBuilder::build_with_handoff(
        metadata,
        &instruction_entries,
        handoff_dir.as_deref(),
    ));
    config.volatile_user_tail = graph_brief;
    let provider_tool_base = authorized_tool_definitions(
        &dependencies.tool_factory,
        grant.as_ref(),
        mobile_use_active,
    );
    config.provider_local_web_tools = provider_tool_base
        .iter()
        .filter(|definition| is_local_web_tool(&definition.name))
        .cloned()
        .collect();
    config.provider_tool_base = Some(provider_tool_base);
    config.install_provider_derived_request_state(&provider_request_state);
    // Instruct pipe: the tools ride the wire as minimal stubs; their signatures
    // and semantics ride the cached system prompt as one compact manual for the
    // exact advertised set.
    if let Some(system_prompt) = config.system_prompt.as_mut() {
        system_prompt.push_str(&tool_manual(&config.tools));
    }
    let auth_scope = credential_surface_name(resolved.provider.credential_surface()).to_owned();
    let account_scope = resolved
        .account_alias
        .as_deref()
        .map(haider_protocol::ids::CredentialAlias::new);
    config.reasoning_settings = serde_json::to_string(&serde_json::json!({
        "effort": metadata.effort,
        "fast": metadata.fast,
    }))
    .unwrap_or_default();
    config.usage_scope = usage_scope_for(
        &resolved.provider_name,
        &config.model,
        account_scope.clone(),
        &auth_scope,
        &config.reasoning_settings,
        &config.system_prompt,
        &config.tools,
    );
    stamp_usage_lane_dimensions(
        &mut config.usage_scope,
        resolved.provider.usage_lane_dimensions(),
    );
    config.usage_scope.cache_boundaries = Some(CacheBoundaryIdentity {
        instructions: digest_json(
            &instructions
                .as_ref()
                .map(LoadedProjectInstructions::fact)
                .unwrap_or_default(),
        ),
        tool_pack: canonical_tool_definitions_digest(&config.tools),
        system_version: SystemPromptBuilder::VERSION.to_owned(),
        web_tools: format!(
            "anthropic_web_tools={} openai_alpha_search={}",
            web_degrade.anthropic_web_tools, web_degrade.openai_alpha_search
        ),
        reasoning_settings: config.reasoning_settings.clone(),
    });
    surface_request_cache_transitions(
        lease,
        device_id,
        &accepted.run_id,
        accepted.branch_id.as_ref(),
        &event_ids,
        metadata,
        &config.usage_scope,
    )
    .await?;
    let (previous_cache_request, cache_initial_rewarm) =
        prior_cache_request_context(lease, &config.usage_scope).await?;
    config.cache_diagnostic_key = lease.hub().cache_diagnostic_key();
    config.cache_previous_request = previous_cache_request;
    config.cache_initial_rewarm = cache_initial_rewarm;
    config.cache_reuse_gap_ms =
        prior_cache_domain_gap_ms(lease, &accepted.run_id, &config.usage_scope).await?;
    config.cache_stable_history_end = Some(compiled_stable_history_end);
    config.cache_current_user_start = Some(compiled_current_user_start);
    config.cache_compaction_summary_end = compiled_compaction_summary_end;
    // A coding turn with an advertised tool pack is expected to reuse a
    // sufficiently large immutable prefix for at least a tool loop and a
    // later turn. Toolless lanes retain the zero-reuse safe fallback.
    config.cache_expected_later_reads = u32::from(!config.tools.is_empty()) * 2;
    config.usage_account = account_scope;
    // W6c children retain the spawn tool. The coordinator derives their
    // durable depth from the parent delegation and returns a typed tool
    // result at the cap; hiding the tool would turn that recoverable model
    // decision into provider-specific behavior. G1: children do NOT retain
    // `todo_write` — the plan surface is root-only (L5).
    config.attachments = attachments;
    // Round 5 (known, accepted): these lane facts FREEZE at turn setup.
    // Mid-turn account rotation or web-tool degradation can drift the live
    // lane away from them; the replay then simply misses the cache (and may
    // take the degraded fallback) — a cost edge, never a correctness one.
    // Re-resolving at compact time needs live-lane threading; tracked.
    config.context_compactor = Some(Arc::new(DaemonContextCompactor {
        store: lease.clone(),
        provider: Arc::clone(&resolved.provider),
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        context_window: config.context_window,
        reserved_output_tokens: config.reserved_output_tokens,
        post_compaction_system_prompt: config.system_prompt.clone(),
        post_compaction_tools: config.tools.clone(),
        reasoning_settings: config.reasoning_settings.clone(),
        cache_expected_later_reads: config.cache_expected_later_reads,
        cache_reuse_gap_ms: config.cache_reuse_gap_ms,
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
        agent_id: config.agent_id.clone(),
        branch_id: accepted.branch_id.clone(),
        usage_scope: config.usage_scope.clone(),
        usage_account: config.usage_account.clone(),
    }));
    config.finalization_guard = Some(Arc::new(DaemonGraphFinalizationGuard {
        store: lease.clone(),
        branch_id: accepted.branch_id.clone(),
        device_id: device_id.clone(),
    }));
    config.rotation_budget_consumed = resolved.rotation_budget_consumed;
    config.initial_rotation = resolved.initial_rotation;
    config.provider_attempt_resolver = resolved.attempt_resolver;
    config.compaction_promotion = resolved.compaction_promotion;
    config.provider_pair_switch_committer = Some(Arc::new(DaemonProviderPairSwitchCommitter {
        store: lease.clone(),
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
    }));
    config.supervisor_commits_cancelled = true;
    // Last uncancellable startup boundary: provider/tool resolution is done,
    // but the harness actor has not been spawned or submitted. A cancellation
    // committed while either factory was awaited aborts here. The worker
    // append transition gate remains the atomic backstop for a later tie.
    if cancellation_fences_start(durable_run_state(lease, &accepted.run_id).await) {
        if let Some(dispatcher) = dispatcher.as_ref() {
            let _ = dispatcher.close().await;
        }
        return Err(cancellation_fenced_start());
    }
    let (actor, harness) = HarnessActor::new_with_dispatcher_and_artifacts(
        config,
        resolved.provider,
        Arc::new(lease.clone()),
        dispatcher.clone(),
        Some(Arc::new(lease.clone())),
    );
    match (checkpoint.as_ref(), partial_stream.as_ref()) {
        (Some(checkpoint), None) => {
            lease
                .register_recovered_harness(
                    harness.clone(),
                    checkpoint.menu.id.clone(),
                    checkpoint.request_seq,
                    checkpoint.opening_generation,
                )
                .await
                .map_err(hub_error)?;
        }
        (None, Some(checkpoint)) => {
            lease
                .register_recovered_harness(
                    harness.clone(),
                    checkpoint.menu.id.clone(),
                    checkpoint.request_seq,
                    checkpoint.opening_generation,
                )
                .await
                .map_err(hub_error)?;
        }
        (None, None) => {
            lease
                .register_harness(harness.clone())
                .await
                .map_err(hub_error)?;
        }
        (Some(_), Some(_)) => {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                "recovered turn contains two menu checkpoint kinds",
                false,
            ));
        }
    }
    if committed_answer.is_none() {
        let menu = checkpoint
            .as_ref()
            .map(|checkpoint| &checkpoint.menu)
            .or_else(|| partial_stream.as_ref().map(|checkpoint| &checkpoint.menu));
        if let Some(menu) = menu {
            committed_answer =
                find_committed_menu_answer(lease, accepted.branch_id.as_ref(), &menu.id).await?;
        }
    }
    if let Some(answer) = committed_answer {
        harness.apply_committed_menu_event(answer)?;
    }
    let actor = AbortOnDropTask::new(tokio::spawn(actor.run()));
    let submitted = match (checkpoint, partial_stream, child_wait) {
        (Some(checkpoint), None, None) => {
            harness
                .submit_checkpoint_turn(SubmitCheckpointTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        (None, Some(checkpoint), None) => {
            harness
                .submit_partial_stream_turn(SubmitPartialStreamTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        (None, None, Some(checkpoint)) => {
            harness
                .submit_child_wait_turn(SubmitChildWaitTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        (None, None, None) => {
            harness
                .submit_committed_turn(SubmitCommittedTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                })
                .await
        }
        _ => Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            "recovered turn contains multiple checkpoint kinds",
            false,
        )),
    };
    let handle = submitted?;
    // W-B: whether THIS turn declared the anthropic server web tools — the
    // precondition for the invalid-request degrade latch on its outcome.
    let anthropic_web_tools = matches!(
        resolved.provider_name.as_str(),
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    ) && !web_degrade.anthropic_web_tools;
    Ok(active_turn(
        accepted.run_id,
        accepted.branch_id,
        harness,
        actor.into_inner(),
        dispatcher,
        handle,
        anthropic_web_tools,
    ))
}

/// Whether a provider's reported cache-read count is already included in its
/// input count. DeepSeek is deliberately disjoint: its adapter maps cache
/// misses to `input` and cache hits to `cached` from separate wire fields.
pub(crate) fn cached_input_is_subset_for_provider(provider: &str) -> bool {
    !matches!(
        provider,
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME | DEEPSEEK_PROVIDER_NAME
    )
}

fn credential_surface_name(surface: ProviderCredentialSurface) -> &'static str {
    match surface {
        ProviderCredentialSurface::Opaque => "opaque",
        ProviderCredentialSurface::ApiKey => "api_key",
        ProviderCredentialSurface::OAuthSubscriptionBearer => "oauth_subscription",
        ProviderCredentialSurface::CloudBearer => "cloud_bearer",
    }
}

fn digest_json(value: &(impl serde::Serialize + ?Sized)) -> String {
    serde_json::to_vec(value).map_or_else(
        |_| blake3::hash(b"serialization-error").to_hex().to_string(),
        |bytes| blake3::hash(&bytes).to_hex().to_string(),
    )
}

fn usage_scope_for(
    provider: &str,
    model: &str,
    account_scope: Option<haider_protocol::ids::CredentialAlias>,
    auth_scope: &str,
    reasoning_settings: &str,
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
) -> UsageScope {
    let cache_epoch = digest_json(&serde_json::json!({
        "provider": provider,
        "model": model,
        "account": account_scope.as_ref(),
        "auth": auth_scope,
        "reasoning": reasoning_settings,
        "system": digest_json(system_prompt),
        "tools": canonical_tool_definitions_digest(tools),
    }));
    UsageScope {
        provider: provider.to_owned(),
        model: model.to_owned(),
        account_scope,
        auth_scope: auth_scope.to_owned(),
        api_family: None,
        effort: None,
        speed: None,
        cache_epoch,
        stable_prefix_tokens: 0,
        cache_boundaries: None,
        request_kind: UsageRequestKind::MainTurn,
        run: None,
        agent: None,
        prefix_digests: None,
    }
}

fn stamp_usage_lane_dimensions(
    scope: &mut UsageScope,
    dimensions: haider_protocol::provider::UsageLaneDimensions,
) {
    scope.api_family = dimensions.api_family;
    scope.effort = dimensions.effort;
    scope.speed = dimensions.speed;
}

/// Returns the observed wall-clock gap since the latest completed request in
/// the same provider/model/account/auth domain. This reads existing usage
/// telemetry only; it adds no journal facts and unknown clocks/history retain
/// the conservative short-TTL fallback.
async fn prior_cache_domain_gap_ms(
    store: &HubStoreHandle,
    current_run: &RunId,
    scope: &UsageScope,
) -> Result<Option<u64>, HaiderError> {
    let mut cursor = 0_u64;
    let mut latest = None::<u64>;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() == Some(current_run) {
                continue;
            }
            let Ok(EventPayload::Usage(usage)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            let Some(previous) = usage.scope else {
                continue;
            };
            if previous.request_kind == UsageRequestKind::MainTurn
                && previous.provider == scope.provider
                && previous.model == scope.model
                && previous.account_scope == scope.account_scope
                && previous.auth_scope == scope.auth_scope
            {
                latest = Some(latest.map_or(envelope.committed_at_ms, |timestamp| {
                    timestamp.max(envelope.committed_at_ms)
                }));
            }
        }
    }
    let Some(latest) = latest.filter(|timestamp| *timestamp > 0) else {
        return Ok(None);
    };
    let Some(now) = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
    else {
        return Ok(None);
    };
    Ok(Some(now.saturating_sub(latest)))
}

/// E2 — every `plan` proposal body the human ACCEPTED on this branch. The
/// scan pairs durable MenuOpened(origin="plan") bodies with their committed
/// accept answers (key preferred; index 0 is the accept slot).
async fn accepted_plan_bodies(
    store: &HubStoreHandle,
    branch_id: Option<&BranchId>,
) -> Result<Vec<String>, HaiderError> {
    let mut opened: HashMap<haider_protocol::ids::MenuId, String> = HashMap::new();
    let mut accepted = Vec::new();
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(accepted);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.branch_id.as_ref() != branch_id {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            match payload {
                EventPayload::MenuOpened(menu) if menu.origin == "plan" => {
                    opened.insert(menu.id.clone(), menu.body.join("\n"));
                }
                EventPayload::MenuAnswered(answer) => {
                    let accepts = match answer.option_key.as_deref() {
                        Some(key) => key == "accept",
                        None => answer.option_index == 0,
                    };
                    if accepts && let Some(body) = opened.get(&answer.menu) {
                        accepted.push(body.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

async fn find_committed_menu_answer(
    store: &HubStoreHandle,
    branch_id: Option<&BranchId>,
    menu_id: &haider_protocol::ids::MenuId,
) -> Result<Option<haider_protocol::envelope::RawEnvelope>, HaiderError> {
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(None);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        if let Some(answer) = page.into_iter().find(|envelope| {
            envelope.branch_id.as_ref() == branch_id
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                |payload| matches!(payload, EventPayload::MenuAnswered(answer) if answer.menu == *menu_id),
            )
        }) {
            return Ok(Some(answer));
        }
    }
}

/// The ONE header shape a File attachment inlines with (G2): the model sees
/// the filename and line count, then the verbatim UTF-8 content. Both the
/// main prompt lane and the compaction lane call this — parity by
/// construction, not by copy.
fn file_attachment_text(name: &str, lines: u32, text: &str) -> String {
    format!("<file name=\"{name}\" lines=\"{lines}\">\n{text}\n</file>")
}

fn pdf_attachment_text(name: &str, pages: u32, text: &str) -> String {
    format!("<file name=\"{name}\" pages=\"{pages}\" source=\"pdf\">\n{text}\n</file>")
}

fn pdf_extraction_error(error: haider_pdf::PdfError) -> HaiderError {
    let (subcode, title) = match error.kind {
        haider_pdf::PdfErrorKind::Encrypted => ("pdf-encrypted", "PDF is encrypted"),
        haider_pdf::PdfErrorKind::NoExtractableText => {
            ("pdf-no-extractable-text", "PDF has no text layer")
        }
        haider_pdf::PdfErrorKind::Malformed => ("pdf-malformed", "PDF could not be read"),
        haider_pdf::PdfErrorKind::Unsupported => (
            "pdf-extraction-unsupported",
            "PDF text encoding is unsupported",
        ),
        haider_pdf::PdfErrorKind::DecompressionLimit => {
            ("pdf-extraction-too-large", "PDF page content is too large")
        }
    };
    HaiderError::new(ErrorCode::InvalidArgument, error.message.clone(), false).with_presentation(
        ErrorPresentation::new(
            subcode,
            title,
            error.message,
            ErrorScope::Turn,
            [ErrorAction::None],
        ),
    )
}

async fn extract_pdf_attachment(
    bytes: Vec<u8>,
    name: &str,
    pages: u32,
) -> Result<String, HaiderError> {
    let extracted = tokio::task::spawn_blocking(move || haider_pdf::extract_text_bounded(&bytes))
        .await
        .map_err(|_| {
            HaiderError::new(
                ErrorCode::ProviderError,
                "PDF extraction worker stopped unexpectedly",
                true,
            )
        })?
        .map_err(pdf_extraction_error)?;
    if extracted.total_pages != pages {
        return Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            "PDF page count changed after admission",
            false,
        ));
    }
    Ok(pdf_attachment_text(name, pages, &extracted.text))
}

async fn resolve_prompt_attachments(
    store: &dyn haider_core::ArtifactReader,
    messages: &mut [Message],
    vision: FeatureResolve,
    pdf_documents: FeatureResolve,
) -> Result<Vec<ResolvedAttachment>, HaiderError> {
    validate_durable_tool_images(store, messages).await?;
    apply_tool_result_image_budget(messages);
    let mut resolved = Vec::<ResolvedAttachment>::new();
    for message in &mut *messages {
        for block in &mut message.blocks {
            match block {
                haider_protocol::provider::Block::ToolResult { images, .. } => {
                    for image in images {
                        if vision == FeatureResolve::Unsupported {
                            continue;
                        }
                        if resolved
                            .iter()
                            .any(|attachment| attachment.artifact == image.artifact)
                        {
                            continue;
                        }
                        let bytes = store.read_artifact(&image.artifact).await.map_err(|_| {
                            HaiderError::new(
                                ErrorCode::StoreCorrupt,
                                format!("tool image {} is missing from the CAS", image.artifact),
                                false,
                            )
                        })?;
                        resolved.push(ResolvedAttachment {
                            artifact: image.artifact.clone(),
                            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        });
                    }
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { artifact, .. },
                ) => {
                    if resolved
                        .iter()
                        .any(|attachment| attachment.artifact.as_str() == artifact.as_str())
                    {
                        continue;
                    }
                    let artifact = artifact.clone();
                    let bytes = store.read_artifact(&artifact).await?;
                    resolved.push(ResolvedAttachment {
                        artifact,
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    });
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. },
                ) => {
                    let artifact = artifact.clone();
                    let bytes = store.read_artifact(&artifact).await?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("pasted-text attachment {artifact} is not UTF-8"),
                            false,
                        )
                    })?;
                    *block = haider_protocol::provider::Block::Text { text };
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::File {
                        artifact,
                        name,
                        lines,
                    },
                ) => {
                    // G2: a text-file attachment is inlined IN PLACE like
                    // PastedText, with a header naming the file — providers
                    // must never see a File block.
                    let artifact = artifact.clone();
                    let bytes = store.read_artifact(&artifact).await?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("file attachment {artifact} is not UTF-8"),
                            false,
                        )
                    })?;
                    *block = haider_protocol::provider::Block::Text {
                        text: file_attachment_text(name, *lines, &text),
                    };
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Pdf {
                        artifact,
                        name,
                        pages,
                        delivery,
                    },
                ) => {
                    let artifact = artifact.clone();
                    let bytes = store.read_artifact(&artifact).await?;
                    match delivery {
                        haider_protocol::tool::PdfDeliveryMode::NativeDocument => {
                            if pdf_documents != FeatureResolve::Native {
                                return Err(HaiderError::new(
                                    ErrorCode::InvalidArgument,
                                    "the durable PDF payload requires native document support from the selected provider",
                                    false,
                                )
                                .with_presentation(
                                    haider_protocol::error::ErrorPresentation::new(
                                        "pdf-native-document-unsupported",
                                        "Provider cannot receive this PDF",
                                        "This turn was admitted for native PDF delivery, but the selected provider no longer supports native document blocks. Switch back to the original provider or attach the PDF again.",
                                        haider_protocol::error::ErrorScope::Turn,
                                        [haider_protocol::error::ErrorAction::None],
                                    ),
                                ));
                            }
                            if !resolved
                                .iter()
                                .any(|attachment| attachment.artifact.as_str() == artifact.as_str())
                            {
                                resolved.push(ResolvedAttachment {
                                    artifact,
                                    data_base64: base64::engine::general_purpose::STANDARD
                                        .encode(bytes),
                                });
                            }
                        }
                        haider_protocol::tool::PdfDeliveryMode::ExtractedText => {
                            *block = haider_protocol::provider::Block::Text {
                                text: extract_pdf_attachment(bytes, name, *pages).await?,
                            };
                        }
                    }
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Skill { name, .. },
                ) => {
                    let name = name.clone();
                    return Err(HaiderError::new(
                        ErrorCode::InvalidArgument,
                        format!("skill attachment `{name}` is reserved but not yet supported"),
                        false,
                    ));
                }
                _ => {}
            }
        }
    }
    if vision == FeatureResolve::Unsupported {
        degrade_tool_result_images_to_placeholders(messages);
    }
    Ok(resolved)
}

async fn validate_durable_tool_images<R>(store: &R, messages: &[Message]) -> Result<(), HaiderError>
where
    R: haider_core::ArtifactReader + ?Sized,
{
    let mut validated = Vec::<haider_protocol::tool::ImageBlockRef>::new();
    let images = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            haider_protocol::provider::Block::ToolResult { images, .. } => Some(images.as_slice()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    for image in images {
        if !matches!(image.media_type.as_str(), "image/png" | "image/jpeg")
            || image.width == 0
            || image.height == 0
            || image.width > haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_DIMENSION
            || image.height > haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_DIMENSION
            || image.byte_len == 0
            || image.byte_len > haider_protocol::tool::TOOL_RESULT_IMAGE_MAX_BYTES
        {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "tool image {} carries invalid bounded metadata",
                    image.artifact
                ),
                false,
            ));
        }
        if let Some(existing) = validated
            .iter()
            .find(|existing| existing.artifact == image.artifact)
        {
            if existing != &image {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("tool image {} has conflicting metadata", image.artifact),
                    false,
                ));
            }
            continue;
        }
        let bytes = store.read_artifact(&image.artifact).await.map_err(|_| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("tool image {} is missing from the CAS", image.artifact),
                false,
            )
        })?;
        haider_store::validate_image_block(&bytes, &image).map_err(|_| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "tool image {} does not match its bounded CAS metadata",
                    image.artifact
                ),
                false,
            )
        })?;
        validated.push(image);
    }
    Ok(())
}

async fn prepare_tool_images_for_text_only_request<R>(
    store: &R,
    messages: &mut [Message],
) -> Result<(), HaiderError>
where
    R: haider_core::ArtifactReader + ?Sized,
{
    validate_durable_tool_images(store, messages).await?;
    apply_tool_result_image_budget(messages);
    degrade_tool_result_images_to_placeholders(messages);
    Ok(())
}

async fn prepare_compaction_messages(
    store: &HubStoreHandle,
    messages: &mut [Message],
) -> Result<(), HaiderError> {
    for message in messages {
        let mut index = 0;
        while index < message.blocks.len() {
            match message.blocks[index].clone() {
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { .. },
                ) => {
                    message.blocks.remove(index);
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. },
                ) => {
                    let bytes = store.get_artifact(artifact.clone()).await?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("pasted-text attachment {artifact} is not UTF-8"),
                            false,
                        )
                    })?;
                    message.blocks[index] = haider_protocol::provider::Block::Text { text };
                    index += 1;
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Pdf {
                        artifact,
                        name,
                        pages,
                        ..
                    },
                ) => {
                    let bytes = store.get_artifact(artifact).await?;
                    match extract_pdf_attachment(bytes, &name, pages).await {
                        Ok(text) => {
                            message.blocks[index] = haider_protocol::provider::Block::Text { text };
                            index += 1;
                        }
                        // A native-only scanned/encrypted PDF was valid for
                        // its original turn; compaction omits it just as it
                        // omits image attachments instead of retroactively
                        // failing the session.
                        Err(_) => {
                            message.blocks.remove(index);
                        }
                    }
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::File {
                        artifact,
                        name,
                        lines,
                    },
                ) => {
                    // G2 compaction parity: the summarization request sees
                    // the same inlined `<file>` text the main lane sends.
                    let bytes = store.get_artifact(artifact.clone()).await?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("file attachment {artifact} is not UTF-8"),
                            false,
                        )
                    })?;
                    message.blocks[index] = haider_protocol::provider::Block::Text {
                        text: file_attachment_text(&name, lines, &text),
                    };
                    index += 1;
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Skill { name, .. },
                ) => {
                    return Err(HaiderError::new(
                        ErrorCode::InvalidArgument,
                        format!("skill attachment `{name}` is reserved but not yet supported"),
                        false,
                    ));
                }
                _ => index += 1,
            }
        }
    }
    Ok(())
}

fn active_turn(
    run_id: RunId,
    branch_id: Option<BranchId>,
    harness: haider_core::HarnessHandle,
    actor: JoinHandle<()>,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    handle: TurnHandle,
    anthropic_web_tools: bool,
) -> ActiveTurn {
    let cancel = handle.cancel_token();
    ActiveTurn {
        run_id,
        branch_id,
        cancel,
        outcome: Box::pin(handle.wait()),
        harness,
        dispatcher,
        actor: Some(actor),
        anthropic_web_tools,
    }
}

struct AbortOnDropTask(Option<JoinHandle<()>>);

impl AbortOnDropTask {
    fn new(task: JoinHandle<()>) -> Self {
        Self(Some(task))
    }

    fn into_inner(mut self) -> JoinHandle<()> {
        let Some(task) = self.0.take() else {
            unreachable!("abort-on-drop task can be disarmed only once");
        };
        task
    }
}

impl Drop for AbortOnDropTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

async fn append_failure(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    error: HaiderError,
) -> Result<(), HaiderError> {
    append_payloads(
        store,
        device_id,
        run_id,
        branch_id,
        event_ids,
        vec![
            EventPayload::RunFailed {
                code: error.code,
                message: sanitized_failure_message(&error.message),
                retryable: error.retryable,
                presentation: Some(presentation_for_haider_error(&error)),
            },
            EventPayload::RunState(RunState::Errored),
        ],
    )
    .await
}

async fn append_run_state(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    state: RunState,
) -> Result<(), HaiderError> {
    append_payloads(
        store,
        device_id,
        run_id,
        branch_id,
        event_ids,
        vec![EventPayload::RunState(state)],
    )
    .await
}

/// Offers the aggregate `Idle` settle. Deliberately run-agnostic: aggregate
/// `SessionState` envelopes carry no run id, and whether Idle actually
/// commits is decided durably by `Store::settle_session_idle` (all runs
/// terminal), never by this caller's view of which run just finished.
async fn append_session_idle(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    interrupted: bool,
) -> Result<(), HaiderError> {
    let envelope = supervisor_envelope(
        store,
        device_id,
        None,
        None,
        event_ids.next(),
        EventPayload::SessionState(SessionState::Idle { interrupted }),
    )?;
    store.settle_idle(envelope).await.map(|_| ())
}

async fn append_payloads(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    payloads: Vec<EventPayload>,
) -> Result<(), HaiderError> {
    let mut envelopes = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let payload_run_id =
            (!matches!(payload, EventPayload::SessionState(_))).then(|| run_id.clone());
        envelopes.push(supervisor_envelope(
            store,
            device_id,
            branch_id.cloned(),
            payload_run_id,
            event_ids.next(),
            payload,
        )?);
    }
    haider_core::StoreHandle::append(store, &mut envelopes).await?;
    Ok(())
}

/// Commits the terminal direct-command item as prompt-visible immediately
/// before the ordinary omitted `Done` state. Output deltas use the same
/// visibility, so the prompt compiler can reconstruct exactly one bounded
/// user-command record without making model-initiated process output visible.
async fn append_shell_completion(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    event_ids: &EventIdGenerator,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    completed: EventPayload,
) -> Result<(), HaiderError> {
    append_shell_payloads(
        store,
        device_id,
        run_id,
        branch_id,
        agent_id,
        event_ids,
        vec![completed, EventPayload::RunState(RunState::Done)],
    )
    .await
}

/// Shell terminalization is the only recovery closure that exposes its
/// completed command item to later prompts. Every other recovery payload
/// remains omitted, so cancelled/failed model turns cannot leak partial tool
/// history through this direct-user seam.
async fn append_shell_payloads(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    event_ids: &EventIdGenerator,
    payloads: Vec<EventPayload>,
) -> Result<(), HaiderError> {
    let mut envelopes = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let prompt = matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::CommandExecution { .. },
                ..
            })
        )
        .then_some(PromptRender::Verbatim);
        let mut envelope = supervisor_envelope(
            store,
            device_id,
            branch_id.cloned(),
            Some(run_id.clone()),
            event_ids.next(),
            payload,
        )?;
        envelope.agent_id = agent_id.cloned();
        if let Some(prompt) = prompt {
            envelope.render.prompt = prompt;
        }
        envelopes.push(envelope);
    }
    StoreHandle::append(store, &mut envelopes).await?;
    Ok(())
}

/// Journals the effective instruction semantics only when they change. A
/// prior unchanged non-empty fact remains the proof for later turns; an empty
/// fact is emitted when files disappear so recovery does not inherit stale
/// policy. Recovered work uses the latest exact same-run fact first.
async fn journal_project_instructions_if_changed(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    agent_id: Option<&AgentId>,
    event_ids: &EventIdGenerator,
    loaded: Option<&LoadedProjectInstructions>,
) -> Result<(), HaiderError> {
    let current = loaded.map_or_else(ProjectInstructionsLoaded::default, |loaded| loaded.fact());
    let (latest, same_run) = project_instruction_fact_history(store, run_id, branch_id).await?;
    let previous = same_run.as_ref().or(latest.as_ref());
    let changed = previous.map_or(!current.files.is_empty(), |previous| previous != &current);
    if !changed {
        return Ok(());
    }

    let payload = current.to_payload_value().map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot serialize project-instruction fact: {error}"),
            false,
        )
    })?;
    let mut envelope = [haider_protocol::envelope::RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: event_ids.next(),
        seq: 0,
        session_id: store.session_id().clone(),
        branch_id: branch_id.cloned(),
        run_id: Some(run_id.clone()),
        agent_id: agent_id.cloned(),
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }];
    StoreHandle::append(store, &mut envelope).await?;
    if previous.is_some() {
        let previous_scope = latest_main_usage_scope(store).await?;
        let stable = previous_scope
            .as_ref()
            .map_or(0, |scope| scope.stable_prefix_tokens);
        let estimate = previous_scope.as_ref().and_then(|scope| {
            haider_provider::estimate_cache_rewarm_cost_usd(
                &scope.provider,
                &scope.model,
                stable,
                haider_provider::CacheWriteTtl::Default,
            )
        });
        let rewarm_cost_usd = previous_scope
            .as_ref()
            .is_some_and(|scope| scope.auth_scope == "api_key")
            .then(|| estimate.map(|estimate| estimate.extra_input_cost_usd))
            .flatten();
        let transition = CacheEpochTransitionV1 {
            reason: CacheEpochTransitionReason::InstructionsChanged,
            planned: false,
            changed_fields: vec!["instructions".into()],
            invalidated_stable_tokens: stable,
            rewarm_cost_usd,
            rewarm_base_input_equivalent_tokens: estimate
                .map(|estimate| estimate.base_input_equivalent_tokens),
            transition_id: digest_json(&current),
            from_cache_epoch: previous_scope.map(|scope| scope.cache_epoch),
            to_cache_epoch: None,
        };
        let item = transition.extension_item().map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("cannot serialize instruction cache transition: {error}"),
                false,
            )
        })?;
        let item_id = ItemId::new(format!(
            "cache-instructions-{}",
            transition
                .transition_id
                .get(..12)
                .unwrap_or(&transition.transition_id)
        ));
        append_payloads(
            store,
            device_id,
            run_id,
            branch_id,
            event_ids,
            vec![
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
            ],
        )
        .await?;
    }
    Ok(())
}

async fn project_instruction_fact_history(
    store: &HubStoreHandle,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
) -> Result<
    (
        Option<ProjectInstructionsLoaded>,
        Option<ProjectInstructionsLoaded>,
    ),
    HaiderError,
> {
    let mut latest = None;
    let mut same_run = None;
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Some(fact) = ProjectInstructionsLoaded::from_payload_value(&envelope.payload)
            else {
                continue;
            };
            if envelope.run_id.as_ref() == Some(run_id) {
                if envelope.branch_id.as_ref() != branch_id {
                    continue;
                }
                same_run = Some(fact.clone());
            }
            latest = Some(fact);
        }
    }
    Ok((latest, same_run))
}

async fn latest_main_usage_scope(
    store: &HubStoreHandle,
) -> Result<Option<UsageScope>, HaiderError> {
    let mut latest = None;
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Ok(EventPayload::Usage(usage)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            let Some(scope) = usage.scope else {
                continue;
            };
            if scope.request_kind == UsageRequestKind::MainTurn && scope.agent.is_none() {
                latest = Some(scope);
            }
        }
    }
    Ok(latest)
}

async fn prior_cache_request_context(
    store: &HubStoreHandle,
    lane: &UsageScope,
) -> Result<(Option<PreviousCacheRequest>, Option<CacheRewarmReasonV1>), HaiderError> {
    let mut latest_main_usage_seq = 0_u64;
    let mut previous_request = None;
    let mut latest_deliberate_boundary = None::<(u64, CacheRewarmReasonV1)>;
    let mut cursor = 0_u64;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if let Ok(EventPayload::Usage(usage)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
                && usage.scope.as_ref().is_some_and(|scope| {
                    scope.request_kind == UsageRequestKind::MainTurn
                        && scope.agent.is_none()
                        && scope.provider == lane.provider
                        && scope.model == lane.model
                        && scope.account_scope == lane.account_scope
                        && scope.auth_scope == lane.auth_scope
                })
            {
                latest_main_usage_seq = envelope.seq;
                previous_request = usage.request.and_then(|request| {
                    request.cache.map(|cache| PreviousCacheRequest {
                        history_message_count: usize::try_from(cache.history_message_count)
                            .unwrap_or(usize::MAX),
                        breakpoint_hashes: cache.breakpoint_hashes,
                        cache_domain_hash: cache.cache_domain_hash,
                    })
                });
                continue;
            }
            if let Ok(EventPayload::NodeCommitted(TreeNode {
                kind: NodeKind::Compaction { .. },
                ..
            })) = serde_json::from_value::<EventPayload>(envelope.payload.clone())
            {
                latest_deliberate_boundary =
                    Some((envelope.seq, CacheRewarmReasonV1::PlannedCompaction));
                continue;
            }
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                continue;
            };
            let Some(transition) = CacheEpochTransitionV1::from_extension_item(&item) else {
                continue;
            };
            let reason = if transition.reason == CacheEpochTransitionReason::Compaction
                || transition.planned
            {
                CacheRewarmReasonV1::PlannedCompaction
            } else {
                CacheRewarmReasonV1::ConfigurationChange
            };
            latest_deliberate_boundary = Some((envelope.seq, reason));
        }
    }
    let pending = latest_deliberate_boundary
        .filter(|(seq, _)| *seq > latest_main_usage_seq)
        .map(|(_, reason)| reason);
    Ok((previous_request, pending))
}

async fn cache_transition_was_emitted(
    store: &HubStoreHandle,
    transition: &CacheEpochTransitionV1,
) -> Result<bool, HaiderError> {
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 256).await?;
        if page.is_empty() {
            return Ok(false);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        let found = page.into_iter().any(|envelope| {
            let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            else {
                return false;
            };
            CacheEpochTransitionV1::from_extension_item(&item).is_some_and(|existing| {
                existing.reason == transition.reason
                    && existing.transition_id == transition.transition_id
            })
        });
        if found {
            return Ok(true);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn journal_cache_transition_if_new(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    transition: CacheEpochTransitionV1,
) -> Result<(), HaiderError> {
    if cache_transition_was_emitted(store, &transition).await? {
        return Ok(());
    }
    let item = transition.extension_item().map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot serialize cache epoch transition: {error}"),
            false,
        )
    })?;
    let item_id = ItemId::new(format!(
        "cache-transition-{}",
        transition
            .transition_id
            .get(..16)
            .unwrap_or(&transition.transition_id)
    ));
    append_payloads(
        store,
        device_id,
        run_id,
        branch_id,
        event_ids,
        vec![
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: item.clone(),
            }),
            EventPayload::Item(ItemEvent::Completed { item_id, item }),
        ],
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn surface_request_cache_transitions(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    event_ids: &EventIdGenerator,
    metadata: &SessionMetadataV1,
    current: &UsageScope,
) -> Result<(), HaiderError> {
    let previous = latest_main_usage_scope(store).await?;
    let Some(previous) = previous else {
        return Ok(());
    };
    let stable = previous.stable_prefix_tokens;
    let estimate = haider_provider::estimate_cache_rewarm_cost_usd(
        &current.provider,
        &current.model,
        stable,
        haider_provider::CacheWriteTtl::Default,
    );
    let cost = (current.auth_scope == "api_key")
        .then(|| estimate.map(|estimate| estimate.extra_input_cost_usd))
        .flatten();
    let equivalents = estimate.map(|estimate| estimate.base_input_equivalent_tokens);
    let transition =
        |reason, fields: Vec<String>, identity: serde_json::Value| CacheEpochTransitionV1 {
            reason,
            planned: false,
            changed_fields: fields,
            invalidated_stable_tokens: stable,
            rewarm_cost_usd: cost,
            rewarm_base_input_equivalent_tokens: equivalents,
            transition_id: digest_json(&serde_json::json!({
                "reason": reason,
                "from": previous.cache_epoch,
                "to": identity,
            })),
            from_cache_epoch: Some(previous.cache_epoch.clone()),
            to_cache_epoch: Some(current.cache_epoch.clone()),
        };

    let mut config_fields = Vec::new();
    if previous.provider != current.provider {
        config_fields.push("provider".into());
    }
    if previous.model != current.model {
        config_fields.push("model".into());
    }
    if previous.auth_scope != current.auth_scope {
        config_fields.push("auth".into());
    }
    if previous.account_scope != current.account_scope {
        config_fields.push("account".into());
    }
    let prior_reasoning = previous
        .cache_boundaries
        .as_ref()
        .map(|boundaries| boundaries.reasoning_settings.as_str());
    let current_reasoning = current
        .cache_boundaries
        .as_ref()
        .map(|boundaries| boundaries.reasoning_settings.as_str());
    if prior_reasoning != current_reasoning {
        config_fields.push("effort/thinking/fast".into());
    }
    if !config_fields.is_empty() {
        journal_cache_transition_if_new(
            store,
            device_id,
            run_id,
            branch_id,
            event_ids,
            transition(
                CacheEpochTransitionReason::ConfigurationChanged,
                config_fields,
                serde_json::json!({
                    "provider": current.provider,
                    "model": current.model,
                    "auth": current.auth_scope,
                    "account": current.account_scope,
                    "reasoning": current_reasoning,
                }),
            ),
        )
        .await?;
    }

    let previous_boundaries = previous.cache_boundaries.as_ref();
    let current_boundaries = current.cache_boundaries.as_ref();
    let system_version_changed = match (previous_boundaries, current_boundaries) {
        (Some(previous), Some(current)) => previous.system_version != current.system_version,
        _ => metadata.system_prompt_version.as_deref() != Some(SystemPromptBuilder::VERSION),
    };
    if system_version_changed {
        journal_cache_transition_if_new(
            store,
            device_id,
            run_id,
            branch_id,
            event_ids,
            transition(
                CacheEpochTransitionReason::SystemVersionChanged,
                vec!["system_version".into()],
                serde_json::json!(SystemPromptBuilder::VERSION),
            ),
        )
        .await?;
    }
    if let (Some(previous), Some(current)) = (previous_boundaries, current_boundaries) {
        if previous.tool_pack != current.tool_pack {
            journal_cache_transition_if_new(
                store,
                device_id,
                run_id,
                branch_id,
                event_ids,
                transition(
                    CacheEpochTransitionReason::ToolPackChanged,
                    vec!["tools".into()],
                    serde_json::json!(&current.tool_pack),
                ),
            )
            .await?;
        }
        if previous.web_tools != current.web_tools && current.web_tools.contains("=true") {
            journal_cache_transition_if_new(
                store,
                device_id,
                run_id,
                branch_id,
                event_ids,
                transition(
                    CacheEpochTransitionReason::WebToolDegradation,
                    vec!["web_tools".into()],
                    serde_json::json!(&current.web_tools),
                ),
            )
            .await?;
        }
    }
    Ok(())
}

fn supervisor_envelope(
    store: &HubStoreHandle,
    device_id: &DeviceId,
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    event_id: EventId,
    payload: EventPayload,
) -> Result<haider_protocol::envelope::RawEnvelope, HaiderError> {
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id,
        seq: 0,
        session_id: store.session_id().clone(),
        branch_id: if matches!(payload, EventPayload::SessionState(_)) {
            None
        } else {
            branch_id
        },
        run_id,
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("cannot serialize supervisor event: {error}"),
                false,
            )
        })?,
    })
}

fn manager_stopped() -> HaiderError {
    HaiderError::new(ErrorCode::Internal, "worker manager is not running", true)
}

fn manager_busy(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::Busy, message, true)
}

fn manager_try_send(error: mpsc::error::TrySendError<ManagerCommand>) -> HaiderError {
    match error {
        mpsc::error::TrySendError::Full(_) => manager_busy("worker manager queue is full"),
        mpsc::error::TrySendError::Closed(_) => manager_stopped(),
    }
}

fn supervisor_try_send(error: mpsc::error::TrySendError<SupervisorCommand>) -> HaiderError {
    match error {
        mpsc::error::TrySendError::Full(_) => manager_busy("session worker queue is full"),
        mpsc::error::TrySendError::Closed(_) => manager_stopped(),
    }
}

fn cancellation_fenced_start() -> HaiderError {
    HaiderError::new(
        ErrorCode::RunNotActive,
        "turn start was fenced by durable cancellation",
        false,
    )
}

fn cancellation_fences_start(state: Option<RunState>) -> bool {
    matches!(state, Some(RunState::Cancelling | RunState::Cancelled))
}

fn hub_error(error: SessionHubError) -> HaiderError {
    HaiderError::new(ErrorCode::Internal, error.to_string(), true)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod manager_law_tests {
    use super::*;

    #[test]
    fn full_manager_queue_maps_to_typed_busy_without_waiting() {
        let (commands, _receiver) = mpsc::channel(1);
        let (first, _first_response) = oneshot::channel();
        commands
            .try_send(ManagerCommand::Shutdown { completed: first })
            .expect("fills manager queue");
        let (second, _second_response) = oneshot::channel();
        let error = commands
            .try_send(ManagerCommand::Shutdown { completed: second })
            .map_err(manager_try_send)
            .expect_err("full queue rejects immediately");
        assert_eq!(error.code, ErrorCode::Busy);
        assert!(error.retryable);
    }

    #[test]
    fn runtime_closes_worker_admission_before_hub_drain() {
        let runtime = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime.rs"),
        )
        .expect("runtime source");
        let worker = runtime
            .find("worker_handle.begin_draining();")
            .expect("worker admission gate");
        let hub = runtime
            .find("hub.begin_draining();")
            .expect("hub admission gate");
        assert!(worker < hub);
    }

    #[test]
    fn naturally_done_turn_stays_uninterrupted_when_drain_observes_it_late() {
        assert!(!idle_interrupted_after_outcome(
            Some(&RunState::Done),
            Some(&RunState::Done),
        ));
        assert!(idle_interrupted_after_outcome(
            Some(&RunState::Cancelled),
            Some(&RunState::Cancelling),
        ));
    }

    #[test]
    fn durable_cancelling_fences_the_last_harness_start_boundary() {
        assert!(cancellation_fences_start(Some(RunState::Cancelling)));
        assert!(cancellation_fences_start(Some(RunState::Cancelled)));
        assert!(!cancellation_fences_start(Some(RunState::Queued)));
    }
}

// ───────────────── production broker-backed general tools ─────────────────

pub(crate) struct BrokerToolFactory;

#[cfg(test)]
pub(crate) struct InjectedComputerBrokerToolFactory {
    backend: Arc<dyn ComputerBackend>,
    screenshot_redaction: Arc<dyn ScreenshotRedactionPolicy>,
}

#[cfg(test)]
pub(crate) struct InjectedMobileBrokerToolFactory {
    backend: Arc<dyn MobileBackend>,
}

#[cfg(test)]
impl BrokerToolFactory {
    /// Test/integration seam for deterministic computer actions. Production
    /// continues to use the unit factory and the cfg-selected platform backend.
    pub(crate) fn with_computer_backend(
        backend: Arc<dyn ComputerBackend>,
    ) -> InjectedComputerBrokerToolFactory {
        InjectedComputerBrokerToolFactory {
            backend,
            screenshot_redaction: Arc::new(haider_tools::PassthroughScreenshotRedaction),
        }
    }

    /// Test seam that pins the production ordering: redact before CU-1 image
    /// admission, then reuse that exact admitted reference for provider context
    /// and daemon-authored convergence-graph evidence.
    pub(crate) fn with_computer_backend_and_redaction(
        backend: Arc<dyn ComputerBackend>,
        screenshot_redaction: Arc<dyn ScreenshotRedactionPolicy>,
    ) -> InjectedComputerBrokerToolFactory {
        InjectedComputerBrokerToolFactory {
            backend,
            screenshot_redaction,
        }
    }

    /// Test/integration seam for deterministic mobile actions. Production
    /// continues to use the unavailable platform stub in this lane.
    pub(crate) fn with_mobile_backend(
        backend: Arc<dyn MobileBackend>,
    ) -> InjectedMobileBrokerToolFactory {
        InjectedMobileBrokerToolFactory { backend }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisteredToolRoute {
    RequestInput,
    Plan,
    LoomRegister,
    TodoWrite,
    GraphEvidence,
    FsRead,
    FsGlob,
    FsSearch,
    FsWrite,
    FsEdit,
    FsPath,
    ProcessExec,
    WorkflowAuthor,
    SpawnSubagent,
    MessageSubagent,
    TaskOutput,
    TaskKill,
    WebFetch,
    WebSearch,
    Computer,
    Mobile,
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredTool {
    pub(crate) manifest: ToolManifest,
    pub(crate) default: ToolPermissionDefault,
    pub(crate) route: RegisteredToolRoute,
}

fn registered_tool(
    definition: ToolDefinition,
    effects: Vec<EffectClass>,
    dispatch: DispatchMode,
    default: ToolPermissionDefault,
    route: RegisteredToolRoute,
) -> RegisteredTool {
    RegisteredTool {
        manifest: ToolManifest {
            name: definition.name,
            description: definition.description,
            effects,
            dispatch,
            input_schema: definition.input_schema,
        },
        default,
        route,
    }
}

fn registered_manifest(
    manifest: ToolManifest,
    default: ToolPermissionDefault,
    route: RegisteredToolRoute,
) -> RegisteredTool {
    RegisteredTool {
        manifest,
        default,
        route,
    }
}

/// The single daemon-owned public tool registry. Provider definitions,
/// dispatcher routes, policy defaults, and inventory reads all project from
/// these entries. Legacy aliases intentionally live only in
/// `registered_tool_route` and can never be advertised.
pub(crate) fn registered_tools() -> Vec<RegisteredTool> {
    vec![
        registered_tool(
            request_input_definition(),
            vec![],
            DispatchMode::Await,
            ToolPermissionDefault::NotApplicable,
            RegisteredToolRoute::RequestInput,
        ),
        // D4: actor-owned like request_input — presenting a proposal is not
        // a side effect; the run parks on the durable plan menu.
        registered_tool(
            plan_definition(),
            vec![],
            DispatchMode::Await,
            ToolPermissionDefault::NotApplicable,
            RegisteredToolRoute::Plan,
        ),
        // E2: registration is plan-gated, not broker-gated — the ACCEPTED
        // plan carrying the exact registration IS the human authorization,
        // so no effect class rides here.
        registered_tool(
            loom_register_definition(),
            vec![],
            DispatchMode::Await,
            ToolPermissionDefault::NotApplicable,
            RegisteredToolRoute::LoomRegister,
        ),
        {
            // G1: actor-owned like request_input — no brokered effect.
            let manifest = haider_tools::todo_write_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::TodoWrite,
            }
        },
        {
            let manifest = haider_tools::graph_evidence_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::GraphEvidence,
            }
        },
        {
            let manifest = haider_tools::workflow_author_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::WorkflowAuthor,
            }
        },
        registered_manifest(
            haider_tools::fs_read_manifest(),
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsRead,
        ),
        registered_manifest(
            haider_tools::fs_glob_manifest(),
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsGlob,
        ),
        registered_manifest(
            haider_tools::fs_search_manifest(),
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsSearch,
        ),
        registered_manifest(
            haider_tools::fs_write_manifest(),
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsWrite,
        ),
        registered_manifest(
            haider_tools::fs_edit_manifest(),
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsEdit,
        ),
        registered_manifest(
            haider_tools::fs_path_manifest(),
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsPath,
        ),
        registered_tool(
            process_exec_definition(),
            vec![EffectClass::ProcessExec],
            DispatchMode::Await,
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::ProcessExec,
        ),
        {
            let manifest = haider_tools::spawn_subagent_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::Allow,
                route: RegisteredToolRoute::SpawnSubagent,
            }
        },
        {
            let manifest = haider_tools::message_subagent_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::MessageSubagent,
            }
        },
        {
            // W-A: bounded task-output reads are not an effect (the
            // request_input pattern — no broker).
            let manifest = haider_tools::task_output_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::TaskOutput,
            }
        },
        {
            // W-A: killing a task IS an effect under the existing process
            // ceiling; same Ask default as process_exec, and the same
            // session override (`allow_exec`) lifts both together.
            let manifest = haider_tools::task_kill_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::Ask,
                route: RegisteredToolRoute::TaskKill,
            }
        },
        {
            // W-B: the universal LOCAL web fetch IS an effect under the
            // `Network { host }` class — Ask by default, per-host grants
            // from the menu, auto-allowed under the exec override (a
            // process can reach the network anyway, so `allow_exec` is the
            // honest auto-mode gate; delegated children carry it).
            let manifest = haider_tools::web_fetch_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::Ask,
                route: RegisteredToolRoute::WebFetch,
            }
        },
        {
            // W-B: the client web_search rides the SAME subscription
            // credential as the turn itself — provider-credential traffic,
            // not a brokered effect (the request_input pattern). Advertised
            // on responses-lite pairs only (see the derivation seam).
            let manifest = haider_tools::web_search_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::WebSearch,
            }
        },
        {
            // CU-2: observation and control are independently brokered. Ask
            // is fail-closed while preserving the existing permission-menu
            // path; neither class is lifted by `allow_exec`.
            let manifest = haider_tools::computer_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::Ask,
                route: RegisteredToolRoute::Computer,
            }
        },
        {
            // Mobile-use is both capability-gated and effect-brokered. Ask is
            // fail-closed once the session has explicitly activated it.
            let manifest = haider_tools::mobile_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::Ask,
                route: RegisteredToolRoute::Mobile,
            }
        },
    ]
}

/// The broad child request before the durable parent ceiling is applied.
/// `todo_write` remains root-only; every other registered declaration is
/// included and then intersected with the parent grant.
pub(crate) fn default_child_grant() -> Grant {
    let entries = registered_tools();
    Grant {
        tools: entries
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.route,
                    RegisteredToolRoute::TodoWrite
                        | RegisteredToolRoute::GraphEvidence
                        | RegisteredToolRoute::WorkflowAuthor
                        | RegisteredToolRoute::Plan
                        | RegisteredToolRoute::LoomRegister
                        | RegisteredToolRoute::Computer
                        | RegisteredToolRoute::Mobile
                )
            })
            .map(|entry| entry.manifest.name.clone())
            .collect(),
        effect_ceiling: entries
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.route,
                    RegisteredToolRoute::TodoWrite
                        | RegisteredToolRoute::GraphEvidence
                        | RegisteredToolRoute::WorkflowAuthor
                        | RegisteredToolRoute::Plan
                        | RegisteredToolRoute::LoomRegister
                        | RegisteredToolRoute::Computer
                        | RegisteredToolRoute::Mobile
                )
            })
            .flat_map(|entry| entry.manifest.effects.iter().cloned())
            .fold(Vec::new(), |mut effects, effect| {
                if !effects.contains(&effect) {
                    effects.push(effect);
                }
                effects
            }),
    }
}

pub(crate) fn effect_within_grant(grant: &Grant, requested: &EffectClass) -> bool {
    grant
        .effect_ceiling
        .iter()
        .any(|ceiling| match (ceiling, requested) {
            (
                EffectClass::Network { host: ceiling_host },
                EffectClass::Network {
                    host: requested_host,
                },
            ) => {
                // Round 3: hosts compare case-insensitively — registration
                // lowercases declared APIs, and URL parsing lowercases the
                // request host, but the fence never depends on either.
                ceiling_host.is_empty() || ceiling_host.eq_ignore_ascii_case(requested_host)
            }
            _ => ceiling == requested,
        })
}

/// B3 — the least-privilege grant for a TYPED child. A specialist is a LEAF:
/// filesystem work is always granted (artifact output is the point), exec
/// rides only with declared CLIs, the network only with declared APIs — and
/// then HOST-SCOPED per API rather than the family — and child-spawning is
/// never granted.
pub(crate) fn typed_child_grant(record: &haider_protocol::loom::LoomAgentType) -> Grant {
    let mut tools: Vec<String> = [
        "request_input",
        "fs_read",
        "fs_glob",
        "fs_search",
        "fs_write",
        "fs_edit",
        "fs_path",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut effects = vec![EffectClass::FsRead, EffectClass::FsWrite];
    if !record.clis.is_empty() {
        tools.extend(["process_exec", "task_output", "task_kill"].map(str::to_owned));
        effects.push(EffectClass::ProcessExec);
    }
    if !record.apis.is_empty() {
        tools.push("web_fetch".to_owned());
        for api in &record.apis {
            effects.push(EffectClass::Network { host: api.clone() });
        }
    }
    Grant {
        tools,
        effect_ceiling: effects,
    }
}

/// B3 (review round 2) — a typed child's exec authority is its DECLARED
/// CLIs, not "any shell". One declared program per call: chaining
/// metacharacters would smuggle a second program (`curl`!) past both this
/// check and the declared-API host ceiling.
pub(crate) fn cli_scope_admits(clis: &[String], command: &str) -> Result<(), String> {
    // Round 3: process substitution `<(..)`/`>(..)` executes a second
    // program just like `$(..)` — all are chaining shapes.
    let chained = command.contains([';', '|', '&', '`', '\n'])
        || command.contains("$(")
        || command.contains("<(")
        || command.contains(">(");
    if chained {
        return Err(
            "typed grant: command chaining/substitution is outside this agent's CLI scope"
                .to_owned(),
        );
    }
    // Round 3: EXACT first-token match — no basename resolution, so an
    // attacker-writable `./ffmpeg` or `/tmp/ffmpeg` never rides a declared
    // bare name. A declared CLI is the literal token the child may run.
    let first = command.split_whitespace().next().unwrap_or("");
    if clis.iter().any(|cli| cli == first) {
        Ok(())
    } else {
        Err(format!(
            "typed grant: `{first}` is not this agent's declared CLI (declare the exact token)"
        ))
    }
}

/// Parses the daemon-stamped `@type · ` task prefix back to its type id.
/// C3 guarantees the prefix is daemon truth: typed spawns get it stamped,
/// untyped @-cosplay gets stripped — so a match here IS a typed child.
pub(crate) fn loom_task_type_id(task: &str) -> Option<String> {
    let rest = task.strip_prefix('@')?;
    let (id, _) = rest.split_once(" · ")?;
    (!id.is_empty() && !id.contains(char::is_whitespace)).then(|| id.to_owned())
}

/// B2 (review round 2) — the EXPLICIT host scope of a typed grant: `Some`
/// only when the ceiling holds host-scoped Network members and no family
/// wildcard. `None` = unscoped (root sessions, untyped children).
pub(crate) fn scoped_network_hosts(grant: &Grant) -> Option<Vec<String>> {
    let mut hosts = Vec::new();
    for effect in &grant.effect_ceiling {
        if let EffectClass::Network { host } = effect {
            if host.is_empty() {
                return None;
            }
            hosts.push(host.clone());
        }
    }
    (!hosts.is_empty()).then_some(hosts)
}

/// B2/B3 — ADMISSION of a tool by its MANIFEST effect. A manifest names the
/// family (`Network{host:""}`); a typed grant may hold host-SCOPED members.
/// The tool is admitted (validated/advertised/dispatched) when the grant
/// holds ANY member of the family — the per-call host is then bounded at the
/// use site ([`web_fetch_host_allowed`]).
pub(crate) fn grant_admits_manifest_effect(grant: &Grant, effect: &EffectClass) -> bool {
    if effect_within_grant(grant, effect) {
        return true;
    }
    matches!(effect, EffectClass::Network { host } if host.is_empty())
        && grant
            .effect_ceiling
            .iter()
            .any(|ceiling| matches!(ceiling, EffectClass::Network { .. }))
}

/// B2 — the use-site fence: under a grant whose network ceilings are ALL
/// host-scoped, a fetch may touch only a declared host. A family ceiling
/// (empty host) keeps today's behavior.
pub(crate) fn web_fetch_host_allowed(grant: &Grant, host: &str) -> bool {
    effect_within_grant(
        grant,
        &EffectClass::Network {
            host: host.to_owned(),
        },
    )
}

pub(crate) fn intersect_grant(requested: Grant, ceiling: &Grant) -> Grant {
    let registry = registered_tools();
    Grant {
        tools: requested
            .tools
            .into_iter()
            .filter(|name| ceiling.tools.contains(name))
            .filter(|name| {
                registry
                    .iter()
                    .find(|entry| entry.manifest.name == *name)
                    .is_some_and(|entry| {
                        entry
                            .manifest
                            .effects
                            .iter()
                            .all(|effect| grant_admits_manifest_effect(ceiling, effect))
                    })
            })
            .collect(),
        effect_ceiling: requested
            .effect_ceiling
            .into_iter()
            .filter(|effect| effect_within_grant(ceiling, effect))
            .collect(),
    }
}

pub(crate) fn validate_grant(grant: &Grant) -> Result<(), HaiderError> {
    let registry = registered_tools();
    for name in &grant.tools {
        let Some(entry) = registry.iter().find(|entry| entry.manifest.name == *name) else {
            return Err(grant_corrupt(format!(
                "delegated manifest grants unknown tool `{name}`"
            )));
        };
        if !entry
            .manifest
            .effects
            .iter()
            .all(|effect| grant_admits_manifest_effect(grant, effect))
        {
            return Err(grant_corrupt(format!(
                "delegated manifest grants tool `{name}` above its effect ceiling"
            )));
        }
    }
    Ok(())
}

fn grant_corrupt(message: impl Into<String>) -> HaiderError {
    HaiderError::new(
        ErrorCode::StoreCorrupt,
        format!(
            "{}; repair or recreate the delegated session before retrying",
            message.into()
        ),
        false,
    )
}

pub(crate) fn registered_tool_route(name: &str) -> Option<RegisteredToolRoute> {
    if name == "exec" {
        return Some(RegisteredToolRoute::ProcessExec);
    }
    registered_tools()
        .into_iter()
        .find_map(|entry| (entry.manifest.name == name).then_some(entry.route))
}

/// Bounds one web-search result for the tool result (W-B decision 3):
/// 32 KiB on a char boundary with an honest truncation marker.
fn bounded_search_preview(text: String) -> (String, bool) {
    const WEB_SEARCH_RESULT_CAP_BYTES: usize = 32 * 1024;
    if text.len() <= WEB_SEARCH_RESULT_CAP_BYTES {
        return (text, false);
    }
    let mut cut = WEB_SEARCH_RESULT_CAP_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    text.push_str("\n[web_search: output truncated]");
    (text, true)
}

/// Instruct-pipe stub of a tool's JSON Schema (LW-IP). Native tool-calling
/// still needs the STRUCTURE — object-ness, property types, `required`, and
/// `enum` value sets are what a provider validates a tool call against — so
/// those are kept; everything a provider merely DISPLAYS to the model
/// (per-property `description`s) and every bound the daemon re-enforces server
/// side (`minLength`/`maxLength`/`minimum`/`pattern`/`additionalProperties`/
/// combinators) is dropped from the wire and moved, in compact prose, into the
/// system-prompt tool manual. The recursion keeps nested `properties`/`items`
/// so array-of-object and nested-object shapes still guide the model.
pub(crate) fn stub_schema(schema: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = schema else {
        return schema.clone();
    };
    let mut out = serde_json::Map::new();
    for key in ["type", "enum", "required"] {
        if let Some(value) = map.get(key) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(serde_json::Value::Object(properties)) = map.get("properties") {
        let stubbed = properties
            .iter()
            .map(|(name, value)| (name.clone(), stub_schema(value)))
            .collect();
        out.insert("properties".to_owned(), serde_json::Value::Object(stubbed));
    }
    if let Some(items) = map.get("items") {
        out.insert("items".to_owned(), stub_schema(items));
    }
    serde_json::Value::Object(out)
}

/// The one authoritative line for a tool in the system-prompt manual: a typed
/// signature (`?` = optional, `∈` lists an enum's values) plus only the
/// semantics a caller cannot infer from the argument name. Deliberately terse
/// — the point of the instruct pipe is to carry meaning without JSON-Schema
/// syntax tax. `None` only guards an unknown name — every advertised tool is
/// described here (including `computer`, whose stub schema the generic/Gemini
/// path leans on this manual to explain; native providers substitute their own
/// computer tool and ignore both).
pub(crate) fn tool_manual_line(name: &str) -> Option<&'static str> {
    // Enum-valued arguments are NOT enumerated here — the stub schema still
    // carries their allowed values, so listing them again would be dead weight.
    // Each line keeps only the signature plus semantics a caller cannot infer
    // from the argument name.
    Some(match name {
        "computer" => {
            "computer(action, x?, y?, from?, to?, text?, keys?, direction?, amount?, ms?) — control the local desktop. Call screenshot first; x/y and from/to are pixels in the latest screenshot; text=type, keys=shortcut like cmd+shift+4; direction+amount scroll; ms=wait ≤60000"
        }
        "mobile" => {
            "mobile(action, folder?, since?, limit?) — use the explicitly activated mobile capability; sms_read returns SMS data as JSON"
        }
        "fs_read" => {
            "fs_read(path, offset?, limit?) — read a bounded UTF-8 file slice; a directory path lists it"
        }
        "fs_glob" => "fs_glob(pattern, path?) — list workspace files matching a glob",
        "fs_search" => "fs_search(pattern, path?, glob?, case?, mode?) — search file contents",
        "fs_write" => {
            "fs_write(path, content) — create or replace one UTF-8 file, making parent dirs"
        }
        "fs_edit" => {
            "fs_edit(path, edits:[{old, new, replace_all?}]) — atomic anchored replacements on a fresh file; each `old` must be unique unless replace_all"
        }
        "fs_path" => {
            "fs_path(operation, source, destination?, overwrite?) — move/delete/copy; destination is required for move and copy"
        }
        "process_exec" => {
            "process_exec(command, cwd?, background?, name?) — run one shell command in the workspace; background=true returns a task_id, outlives the turn, and its completion posts as a session message (read task_output, stop task_kill)"
        }
        "task_output" => {
            "task_output(task_id, cursor?) — read a background task's output; no cursor = rolling tail, cursor = page from that byte offset"
        }
        "task_kill" => "task_kill(task_id) — terminate a background task's whole process group",
        "web_fetch" => {
            "web_fetch(url, max_bytes?) — fetch a public https (or loopback http) URL and return its readable text"
        }
        "web_search" => "web_search(query) — search the web, returning a bounded text summary",
        "spawn_subagent" => {
            "spawn_subagent(task, prompt, model?, provider?, agent_type?, workflow?, workflow_trigger?, parent_slot?, workflow_author?) — delegate one bounded task to a depth-capped child; task = short label, prompt = full brief; agent_type = a registered Loom specialist (its Job frames the child)"
        }
        "message_subagent" => {
            "message_subagent(agent, message) — steer a running direct child or start an idle one (agent = id returned by spawn_subagent)"
        }
        "todo_write" => {
            "todo_write(items:[{id, text, state, dep?}]) — REPLACE the whole todo list with the complete plan; keep exactly one item processing; dep = id this item is blocked on"
        }
        "graph_evidence" => {
            "graph_evidence(graph_id, node, verdict, detail, slot?, subject_digest?, signal?, workspace_mutation?) — attest an open obligation; node must equal an open obligation; signal/workspace_mutation carry daemon provenance for verified slots"
        }
        "workflow_author" => {
            "workflow_author(template) — replace this workflow child's initial graph with one bounded validated DAG (template: name, version, start_node, nodes)"
        }
        "request_input" => {
            "request_input(kind, title, body?, options?) — ask the user one blocking prompt; options=[{key, label, detail?}] for a choice"
        }
        "loom_register" => {
            "loom_register(kind, source?|record?) — register a Loom workflow (kind=workflow, source=pipe text `name: In -> Out` + node lines) or agent type (kind=agent_type, record={id,name,job,in_type,out_type,clis,apis,skills,scripts,color,glyph}); refused unless a human-ACCEPTED plan body contains the registration content"
        }
        "plan" => {
            "plan(title, body) — present a full markdown plan/proposal for review before acting; the user answers accept / revise (with a note) / reject and the result is {decision, note}. Use for designs, migrations, architectures — anything that deserves approval first"
        }
        _ => return None,
    })
}

/// Builds the system-prompt tool manual for exactly the tools advertised this
/// turn (the grant/provider-filtered set), so a child or a provider that sheds
/// a tool never sees a signature for one it cannot call. Returns `""` when no
/// advertised tool is manual-described (e.g. a computer-only child), leaving
/// the base prompt byte-identical.
/// C1 — the compact typed-node manual for a pinned Loom workflow. Rides the
/// volatile user tail (never durable history) beside the graph brief; each
/// work node names its agent type and task so the model dispatches it with
/// `spawn_subagent(agent_type=..)`. Bounded: task text is already ≤200B/node.
pub(crate) fn loom_run_tail(workflow: &haider_protocol::loom::LoomWorkflow) -> String {
    let mut tail = format!(
        "loom {} rev {} · {} -> {} — run each OPEN node with spawn_subagent(agent_type, task): ",
        workflow.id, workflow.rev, workflow.in_type, workflow.out_type
    );
    let mut first = true;
    for meta in &workflow.meta {
        if !first {
            tail.push_str(" → ");
        }
        first = false;
        tail.push_str(&meta.source_name);
        if let Some(atype) = &meta.agent_type {
            tail.push('@');
            tail.push_str(atype);
        }
        if !meta.task.is_empty() {
            tail.push_str(" \"");
            tail.push_str(&meta.task);
            tail.push('"');
        }
    }
    // Verify-fix C4: aggregate cap — the tail rides EVERY turn's volatile
    // context; per-node bounds do not bound the whole.
    const LOOM_TAIL_MAX_BYTES: usize = 1_200;
    if tail.len() > LOOM_TAIL_MAX_BYTES {
        // The ellipsis lives INSIDE the cap — 1200 means 1200.
        let mut end = LOOM_TAIL_MAX_BYTES - '…'.len_utf8();
        while !tail.is_char_boundary(end) {
            end -= 1;
        }
        tail.truncate(end);
        tail.push('…');
    }
    tail
}

pub(crate) fn tool_manual(tools: &[ToolDefinition]) -> String {
    let lines: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool_manual_line(&tool.name))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut manual = String::from(
        "\n\nTool manual — authoritative call signatures (? marks an optional argument). \
         Each tool's schema lists any enum values and the daemon enforces every argument bound; \
         call tools through the native tool interface:",
    );
    for line in lines {
        manual.push_str("\n- ");
        manual.push_str(line);
    }
    manual
}

fn provider_definition(manifest: ToolManifest) -> ToolDefinition {
    // Instruct pipe: the wire carries only a tool's NAME and a minimal stub
    // schema (structure + enums). Its human-readable description AND every
    // per-property description move into the single system-prompt tool manual,
    // so the model reads one compact manual instead of paying JSON-Schema
    // syntax tax on every tool, every turn. `computer` is stubbed like the
    // rest: Anthropic/OpenAI substitute their native computer tool (this schema
    // is never read there), and the generic/Gemini path is covered by the
    // manual plus the daemon's own `ComputerOperation::from_tool_args` checks.
    ToolDefinition {
        name: manifest.name,
        description: String::new(),
        input_schema: stub_schema(&manifest.input_schema),
    }
}

/// The tool pack one turn advertises (G1, W-B). The plan surface is
/// root-only: a delegated child session never sees `todo_write` — the pinned
/// panel and the Todos history nodes belong to the root planning timeline.
///
/// W-B (decision 8 / LW8): the pack derives from the RESOLVED pair per turn.
/// First-party Anthropic pairs carry the SERVER `web_fetch` tool, so the
/// LOCAL `web_fetch` client tool is withheld there (two tools cannot share
/// the name); every other pair advertises it. Subagents inherit the same
/// derivation — this is the single advertisement seam.
#[cfg(test)]
pub(crate) fn advertised_tool_definitions(
    tool_factory: &Arc<dyn TurnToolFactory>,
    grant: Option<&Grant>,
    provider_name: &str,
    web_degrade: WebCapabilityDegrade,
) -> Vec<ToolDefinition> {
    advertised_tool_definitions_for_mobile_state(
        tool_factory,
        grant,
        provider_name,
        web_degrade,
        false,
    )
}

fn advertised_tool_definitions_for_mobile_state(
    tool_factory: &Arc<dyn TurnToolFactory>,
    grant: Option<&Grant>,
    provider_name: &str,
    web_degrade: WebCapabilityDegrade,
    mobile_use_active: bool,
) -> Vec<ToolDefinition> {
    let mut definitions = authorized_tool_definitions(tool_factory, grant, mobile_use_active);
    let (local_web_tool_names, _) = provider_web_tool_names(provider_name, web_degrade);
    definitions.retain(|definition| {
        !is_local_web_tool(&definition.name)
            || local_web_tool_names
                .iter()
                .any(|name| name == &definition.name)
    });
    definitions
}

fn authorized_tool_definitions(
    tool_factory: &Arc<dyn TurnToolFactory>,
    grant: Option<&Grant>,
    mobile_use_active: bool,
) -> Vec<ToolDefinition> {
    let mut definitions = tool_factory.definitions();
    definitions.retain(|definition| mobile_use_active || definition.name != "mobile");
    if let Some(grant) = grant {
        let registry = registered_tools();
        definitions.retain(|definition| {
            grant.tools.contains(&definition.name)
                && registry
                    .iter()
                    .find(|entry| entry.manifest.name == definition.name)
                    .is_some_and(|entry| {
                        entry
                            .manifest
                            .effects
                            .iter()
                            .all(|effect| grant_admits_manifest_effect(grant, effect))
                    })
        });
    } else {
        definitions.retain(|definition| definition.name != "workflow_author");
    }
    definitions
}

fn is_local_web_tool(name: &str) -> bool {
    matches!(name, "web_fetch" | "web_search")
}

fn provider_web_tool_names(
    provider_name: &str,
    web_degrade: WebCapabilityDegrade,
) -> (Vec<String>, Vec<String>) {
    let native_anthropic_web = matches!(
        provider_name,
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    ) && !web_degrade.anthropic_web_tools;
    let mut local = Vec::new();
    if !native_anthropic_web {
        local.push("web_fetch".to_owned());
    }
    if provider_name == OPENAI_OAUTH_PROVIDER_NAME && !web_degrade.openai_alpha_search {
        local.push("web_search".to_owned());
    }
    let fallback = if native_anthropic_web {
        vec!["web_fetch".to_owned()]
    } else {
        Vec::new()
    };
    (local, fallback)
}

pub(crate) fn provider_derived_request_state(
    provider_name: &str,
    capabilities: &CapabilityDoc,
    web_degrade: WebCapabilityDegrade,
) -> ProviderDerivedRequestState {
    let (local_web_tool_names, provider_fallback_local_web_tool_names) =
        provider_web_tool_names(provider_name, web_degrade);
    ProviderDerivedRequestState {
        tool_result_images_supported: capabilities.vision != FeatureResolve::Unsupported,
        local_web_tool_names,
        provider_fallback_local_web_tool_names,
    }
}

#[async_trait]
impl TurnToolFactory for BrokerToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        registered_tools()
            .into_iter()
            .map(|entry| provider_definition(entry.manifest))
            .collect()
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        let redaction = haider_tools::configured_screenshot_redaction_policy()
            .map_err(|error| tool_error(ToolError::Computer(error)))?;
        create_broker_tool_dispatcher(
            context,
            haider_tools::platform_computer_backend(),
            haider_tools::platform_mobile_backend(),
            redaction,
        )
        .await
    }
}

#[async_trait]
#[cfg(test)]
impl TurnToolFactory for InjectedComputerBrokerToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        registered_tools()
            .into_iter()
            .map(|entry| provider_definition(entry.manifest))
            .collect()
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        create_broker_tool_dispatcher(
            context,
            Arc::clone(&self.backend),
            haider_tools::platform_mobile_backend(),
            Arc::clone(&self.screenshot_redaction),
        )
        .await
    }
}

#[async_trait]
#[cfg(test)]
impl TurnToolFactory for InjectedMobileBrokerToolFactory {
    fn definitions(&self) -> Vec<ToolDefinition> {
        registered_tools()
            .into_iter()
            .map(|entry| provider_definition(entry.manifest))
            .collect()
    }

    async fn create(
        &self,
        context: WorkerToolContext,
    ) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
        create_broker_tool_dispatcher(
            context,
            Arc::new(haider_tools::UnavailableComputerBackend::new("mobile-test")),
            Arc::clone(&self.backend),
            Arc::new(haider_tools::PassthroughScreenshotRedaction),
        )
        .await
    }
}

async fn create_broker_tool_dispatcher(
    context: WorkerToolContext,
    computer: Arc<dyn ComputerBackend>,
    mobile: Arc<dyn MobileBackend>,
    screenshot_redaction: Arc<dyn ScreenshotRedactionPolicy>,
) -> Result<Option<Arc<dyn ToolDispatcher>>, HaiderError> {
    let durable_permissions =
        durable_session_tool_state(&context.store, context.store.session_id()).await?;
    let session_id = context.store.session_id().clone();
    let active_tool_name = Arc::new(StdMutex::new(None));
    let journal = HubJournalSink::new(&context, Arc::clone(&active_tool_name));
    let mut broker = EffectBroker::new(
        Box::new(journal),
        &context.metadata.cwd,
        context.store.session_id().clone(),
        context.store.worker_generation(),
    )
    .map_err(tool_error)?;
    broker
        .restore_freshness(durable_permissions.freshness.clone().into_values())
        .map_err(tool_error)?;
    let mut policy = PermissionPolicy::default();
    for (class, default) in effective_permission_defaults(&context.metadata) {
        match default {
            ToolPermissionDefault::Allow => policy.allow(class),
            ToolPermissionDefault::Ask => policy.ask(class),
            ToolPermissionDefault::Deny => policy.deny(class, "denied by daemon default policy"),
            ToolPermissionDefault::NotApplicable => {}
        }
    }
    for grant in durable_permissions.grants {
        policy.allow_session_grant(grant).map_err(tool_error)?;
    }
    let output = HubCommandOutputContext {
        store: context.store.clone(),
        branch_id: context.branch_id.clone(),
        agent_id: context.agent_id.clone(),
        device_id: context.device_id.clone(),
        event_ids: Arc::clone(&context.event_ids),
    };
    Ok(Some(Arc::new(BrokerToolDispatcher {
        broker: Mutex::new(Some(broker)),
        web_search: context.web_search.clone(),
        computer,
        mobile,
        screenshot_redaction,
        active_computer_turn_cancel: StdMutex::new(None),
        os_permission_menus: Mutex::new(HashMap::new()),
        pending_computer_permissions: Mutex::new(HashMap::new()),
        permission_pollers: StdMutex::new(HashMap::new()),
        policy: Mutex::new(policy),
        cas: Mutex::new(HubArtifactStore {
            store: context.store,
        }),
        ledger: ChangeLedger::new(),
        session_id,
        branch_id: context.branch_id,
        output,
        durable_permission_bindings: durable_permissions.bindings,
        metadata: context.metadata,
        parent_agent_id: context.agent_id,
        delegation: context.delegation,
        tasks: context.tasks,
        grant: context.grant,
        mobile_use_active: context.mobile_use_active,
        cli_scope: context.cli_scope,
        deferred: Mutex::new(HashMap::new()),
        active_tool_name,
    })))
}

pub(crate) fn effective_permission_defaults(
    metadata: &SessionMetadataV1,
) -> Vec<(EffectClass, ToolPermissionDefault)> {
    let overrides = metadata.permission_overrides.unwrap_or_default();
    registered_tools()
        .into_iter()
        .flat_map(|entry| {
            entry.manifest.effects.into_iter().map(move |class| {
                let base = if (overrides.allow_writes && class == EffectClass::FsWrite)
                    || (overrides.allow_exec && class == EffectClass::ProcessExec)
                    // W-B: the exec override is the honest auto-mode gate for
                    // network fetches too — an allowed process can already
                    // reach the network, so hiding fetch behind a second menu
                    // would be security theater. The empty-host template is
                    // the policy's Network class-family rule.
                    || (overrides.allow_exec && matches!(class, EffectClass::Network { .. }))
                {
                    ToolPermissionDefault::Allow
                } else {
                    entry.default
                };
                // Auto-allow mode is the blanket superset: any class still on
                // the Ask path (computer's ScreenObserve/ScreenControl, web
                // fetch, task-kill, and any class a future tool adds) resolves
                // to Allow. A daemon-level fail-closed `Deny` default is NOT
                // lifted, and `NotApplicable` stays a non-effect — auto-allow
                // only ever promotes `Ask` to `Allow`. Deny RULES still win at
                // the broker (denylist is checked before any allow), the OS
                // TCC gate for computer actions is untouched, and every effect
                // is still journaled.
                let default = if overrides.auto_allow && base == ToolPermissionDefault::Ask {
                    ToolPermissionDefault::Allow
                } else {
                    base
                };
                (class, default)
            })
        })
        .collect()
}

struct BrokerToolDispatcher {
    broker: Mutex<Option<EffectBroker>>,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
    computer: Arc<dyn ComputerBackend>,
    mobile: Arc<dyn MobileBackend>,
    screenshot_redaction: Arc<dyn ScreenshotRedactionPolicy>,
    /// Core's authoritative turn cancellation signal for the currently
    /// dispatched computer action. It survives cancellation dropping the
    /// execute future, allowing `close` to distinguish ESC from a panic or
    /// transport failure and preserve honest Cancelled vs Unknown outcomes.
    active_computer_turn_cancel: StdMutex<Option<CancelToken>>,
    os_permission_menus: Mutex<HashMap<MenuId, Menu>>,
    pending_computer_permissions: Mutex<HashMap<MenuId, PendingComputerPermission>>,
    permission_pollers: StdMutex<HashMap<MenuId, JoinHandle<()>>>,
    policy: Mutex<PermissionPolicy>,
    cas: Mutex<HubArtifactStore>,
    ledger: ChangeLedger,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    output: HubCommandOutputContext,
    durable_permission_bindings: HashMap<MenuId, (EffectClass, String)>,
    metadata: SessionMetadataV1,
    parent_agent_id: Option<AgentId>,
    delegation: DelegationHandle,
    tasks: crate::tasks::TaskFacade,
    grant: Option<Grant>,
    mobile_use_active: bool,
    cli_scope: Option<Vec<String>>,
    deferred: Mutex<HashMap<AgentId, DeferredTicket>>,
    /// The journal sink reads this only while `broker` is held. Setting it
    /// after acquiring that mutex keeps concurrent tool calls correctly named.
    active_tool_name: Arc<StdMutex<Option<String>>>,
}

struct ActiveToolName {
    slot: Arc<StdMutex<Option<String>>>,
}

impl ActiveToolName {
    fn set(slot: &Arc<StdMutex<Option<String>>>, name: &str) -> ToolResult<Self> {
        *slot.lock().map_err(|_| ToolError::Runtime {
            message: "effect diagnostic tool-name lock is poisoned".into(),
        })? = Some(name.to_owned());
        Ok(Self {
            slot: Arc::clone(slot),
        })
    }
}

impl Drop for ActiveToolName {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }
}

#[derive(Debug, Clone)]
struct ComputerGraphTarget {
    graph_id: GraphId,
    node: GraphNodeName,
    attempt: u32,
}

#[derive(Debug, Clone)]
struct PendingComputerPermission {
    effect_id: EffectId,
    permission: haider_protocol::permission::SystemPermission,
    pane_name: String,
    settings_url: String,
    restart_required: bool,
}

struct ComputerObservationRecord<'a> {
    run_id: &'a RunId,
    call_id: &'a str,
    intent: &'a EffectIntent,
    target: Option<ComputerGraphTarget>,
    observation: ComputerObservationKind,
    image: &'a ImageBlockRef,
    detail: String,
}

struct CreatedImageScan<'a> {
    run_id: &'a RunId,
    call_id: &'a str,
    tool: &'a str,
    command: &'a str,
    output_preview: &'a str,
    cwd: &'a Path,
    started: SystemTime,
}

impl BrokerToolDispatcher {
    async fn emit_created_images(&self, scan: CreatedImageScan<'_>) -> ToolResult<()> {
        // Verify round 2: relative tokens resolve against the EXEC cwd, but
        // the publication fence is the SESSION workspace root — an exec in
        // workspace/sub writing ../out.png is inside the workspace.
        for path in detect_created_images(
            scan.command,
            scan.output_preview,
            scan.cwd,
            Path::new(&self.metadata.cwd),
            scan.started,
        ) {
            let Some(image) = image_created_payload(
                &path,
                Path::new(&self.metadata.cwd),
                scan.call_id,
                scan.tool,
            )
            .map_err(|error| ToolError::Runtime {
                message: format!("cannot inspect created image {}: {error}", path.display()),
            })?
            else {
                continue;
            };
            self.output.append_image_created(scan.run_id, image).await?;
        }
        Ok(())
    }

    fn permission_menu(
        intent: &EffectIntent,
        permission: haider_protocol::permission::SystemPermission,
        pane_name: &str,
    ) -> Menu {
        Menu {
            id: MenuId::new(format!(
                "computer-os-permission-{}-{}",
                intent.effect,
                permission.as_str()
            )),
            kind: MenuKind::Permission {
                effect_summary: format!("{} requires {pane_name}", intent.summary),
            },
            title: format!("Allow {pane_name}"),
            body: vec![
                "macOS requires a real user grant. Haider opened the native prompt and will continue automatically when the permission is usable."
                    .into(),
            ],
            options: vec![MenuOption {
                key: "retry".into(),
                label: "Retry".into(),
                detail: Some("Recheck the macOS permission now.".into()),
                decision: Some(DecisionKind::AllowOnce),
            }],
            blocking: true,
            scope: MenuScope::Session,
            origin: COMPUTER_PERMISSION_MENU_ORIGIN.into(),
            ttl_ms: None,
            timeout_option: None,
        }
    }

    fn pending_permission(
        intent: &EffectIntent,
        error: &ComputerError,
    ) -> Option<PendingComputerPermission> {
        let ComputerError::PermissionRequired {
            permission,
            settings_pane,
            settings_url,
            restart_required,
            ..
        } = error
        else {
            return None;
        };
        Some(PendingComputerPermission {
            effect_id: intent.effect.clone(),
            permission: *permission,
            pane_name: settings_pane.clone(),
            settings_url: settings_url.clone(),
            restart_required: *restart_required,
        })
    }

    fn permission_needed(
        checkpoint: &RequestInputCheckpoint,
        pending: &PendingComputerPermission,
    ) -> PermissionGrantNeeded {
        PermissionGrantNeeded {
            request_id: checkpoint.menu.id.to_string(),
            menu_id: checkpoint.menu.id.clone(),
            request_seq: checkpoint.request_seq,
            opening_generation: checkpoint.opening_generation,
            call_id: checkpoint.call_id.clone(),
            effect_id: pending.effect_id.clone(),
            permission: pending.permission,
            pane_name: pending.pane_name.clone(),
            settings_url: pending.settings_url.clone(),
            actions: vec![
                PermissionGrantAction::OpenSettings,
                PermissionGrantAction::Retry,
                PermissionGrantAction::RestartDaemon,
            ],
            auto_restart_pending: pending.restart_required,
            poll_timeout_ms: u64::try_from(COMPUTER_PERMISSION_POLL_TIMEOUT.as_millis())
                .unwrap_or(u64::MAX),
        }
    }

    async fn durable_pending_permission(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingComputerPermission>, HaiderError> {
        let mut cursor = 0;
        let mut found = None;
        loop {
            let page = self
                .output
                .store
                .read(&self.session_id, cursor, 256)
                .await?;
            if page.is_empty() {
                return Ok(found);
            }
            cursor = page.last().map_or(cursor, |event| event.seq);
            for event in page {
                let Ok(PermissionEventPayload::PermissionGrantNeeded(needed)) =
                    PermissionEventPayload::from_payload_value(event.payload)
                else {
                    continue;
                };
                if needed.request_id == request_id {
                    found = Some(PendingComputerPermission {
                        effect_id: needed.effect_id,
                        permission: needed.permission,
                        pane_name: needed.pane_name,
                        settings_url: needed.settings_url,
                        restart_required: needed.auto_restart_pending,
                    });
                }
            }
        }
    }

    async fn resolve_os_permission_menu(
        output: &HubCommandOutputContext,
        checkpoint: &RequestInputCheckpoint,
    ) -> Result<MenuResolutionOutcome, HaiderError> {
        output
            .store
            .hub()
            .resolve_hook_menu(MenuResolutionCommand {
                command_id: format!("computer-os-permission-auto-{}", checkpoint.menu.id),
                session_id: output.store.session_id().clone(),
                request_seq: checkpoint.request_seq,
                // Menu CAS identifies the generation that opened the card;
                // the hub elevates this exact registered recovery coordinate
                // when the live lease belongs to a fresh daemon generation.
                worker_generation: checkpoint.opening_generation,
                allow_prior_generation: checkpoint.opening_generation
                    != output.store.worker_generation(),
                answer: MenuAnswer {
                    menu: checkpoint.menu.id.clone(),
                    option_key: Some("retry".into()),
                    option_index: 0,
                    value: None,
                    via: AnswerVia::Hook,
                },
                device_id: output.device_id.clone(),
                input_is_secret_reference: false,
            })
            .await
    }

    async fn activate_computer_permission(
        &self,
        run_id: &RunId,
        checkpoint: &RequestInputCheckpoint,
    ) -> Result<(), HaiderError> {
        if checkpoint.menu.origin != COMPUTER_PERMISSION_MENU_ORIGIN {
            return Ok(());
        }
        if self
            .permission_pollers
            .lock()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "computer permission poller lock is poisoned",
                    false,
                )
            })?
            .contains_key(&checkpoint.menu.id)
        {
            return Ok(());
        }

        let operation_value: serde_json::Value =
            serde_json::from_str(&checkpoint.args).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("recovered computer arguments could not decode: {error}"),
                    false,
                )
            })?;
        let operation = ComputerOperation::from_tool_args(operation_value).map_err(tool_error)?;
        let mut pending = self
            .pending_computer_permissions
            .lock()
            .await
            .get(&checkpoint.menu.id)
            .cloned();
        let recovered = pending.is_none();
        if pending.is_none() {
            pending = self
                .durable_pending_permission(checkpoint.menu.id.as_str())
                .await?;
        }
        let Some(mut pending) = pending else {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                format!(
                    "computer OS-permission checkpoint {} has no durable grant descriptor",
                    checkpoint.menu.id
                ),
                false,
            ));
        };

        if recovered {
            let cancel = ComputerCancelToken::new();
            match self.computer.prepare(operation.action(), &cancel).await {
                Ok(()) => {
                    self.output
                        .append_permission_payload(
                            run_id,
                            PermissionEventPayload::PermissionGrantResolved(
                                PermissionGrantResolved {
                                    request_id: checkpoint.menu.id.to_string(),
                                    permission: pending.permission,
                                    resolution: PermissionGrantResolution::Granted,
                                    retrying_parked_action: true,
                                },
                            ),
                        )
                        .await
                        .map_err(tool_error)?;
                    Self::resolve_os_permission_menu(&self.output, checkpoint).await?;
                    return Ok(());
                }
                Err(error @ ComputerError::PermissionRequired { .. }) => {
                    let ComputerError::PermissionRequired {
                        permission,
                        settings_pane,
                        settings_url,
                        restart_required,
                        ..
                    } = error
                    else {
                        unreachable!("permission error matched above");
                    };
                    pending.permission = permission;
                    pending.pane_name = settings_pane;
                    pending.settings_url = settings_url;
                    pending.restart_required = restart_required;
                }
                Err(error) => return Err(computer_error(error)),
            }
        }

        self.output
            .append_permission_payload(
                run_id,
                PermissionEventPayload::PermissionGrantNeeded(Self::permission_needed(
                    checkpoint, &pending,
                )),
            )
            .await
            .map_err(tool_error)?;

        let backend = Arc::clone(&self.computer);
        let output = self.output.clone();
        let run_id = run_id.clone();
        let checkpoint = checkpoint.clone();
        let menu_id = checkpoint.menu.id.clone();
        let task = tokio::spawn(async move {
            let cancel = ComputerCancelToken::new();
            match backend
                .poll_permission(
                    pending.permission,
                    &cancel,
                    COMPUTER_PERMISSION_POLL_TIMEOUT,
                )
                .await
            {
                Ok(ComputerPermissionPoll::Granted) => {
                    let appended = output
                        .append_permission_payload(
                            &run_id,
                            PermissionEventPayload::PermissionGrantResolved(
                                PermissionGrantResolved {
                                    request_id: checkpoint.menu.id.to_string(),
                                    permission: pending.permission,
                                    resolution: PermissionGrantResolution::Granted,
                                    retrying_parked_action: true,
                                },
                            ),
                        )
                        .await;
                    if appended.is_ok() {
                        let _ = Self::resolve_os_permission_menu(&output, &checkpoint).await;
                    }
                }
                Ok(ComputerPermissionPoll::RestartRequired) => {
                    pending.restart_required = true;
                    let _ = output
                        .append_permission_payload(
                            &run_id,
                            PermissionEventPayload::PermissionGrantNeeded(Self::permission_needed(
                                &checkpoint,
                                &pending,
                            )),
                        )
                        .await;
                }
                Ok(ComputerPermissionPoll::TimedOut) => {
                    // Deliberately leave the durable menu unanswered. Its
                    // Open Settings, Retry, and Restart actions remain live.
                }
                Err(ComputerError::Cancelled) => {
                    let _ = output
                        .append_permission_payload(
                            &run_id,
                            PermissionEventPayload::PermissionGrantResolved(
                                PermissionGrantResolved {
                                    request_id: checkpoint.menu.id.to_string(),
                                    permission: pending.permission,
                                    resolution: PermissionGrantResolution::Cancelled,
                                    retrying_parked_action: false,
                                },
                            ),
                        )
                        .await;
                }
                Err(_) => {}
            }
        });
        self.permission_pollers
            .lock()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "computer permission poller lock is poisoned",
                    false,
                )
            })?
            .insert(menu_id, task);
        Ok(())
    }

    async fn computer_graph_target(&self) -> ToolResult<Option<ComputerGraphTarget>> {
        let status = self
            .output
            .store
            .hub()
            .graph_status(&self.session_id)
            .await
            .map_err(|error| ToolError::Runtime {
                message: format!("cannot snapshot computer graph target: {error}"),
            })?;
        Ok(status.and_then(|status| {
            let node = status.current_node.clone()?;
            (status.phase == GraphPhase::Active && status.node_is_ready(&node)).then_some(
                ComputerGraphTarget {
                    graph_id: status.graph_id,
                    node,
                    attempt: status.attempt,
                },
            )
        }))
    }

    async fn admit_computer_screenshot(
        &self,
        png: Vec<u8>,
        cancel: &ComputerCancelToken,
    ) -> ToolResult<ImageBlockRef> {
        cancel.check().map_err(ToolError::Computer)?;
        let policy = Arc::clone(&self.screenshot_redaction);
        let redacted = tokio::task::spawn_blocking(move || {
            policy.redact_png(&png).map(std::borrow::Cow::into_owned)
        })
        .await
        .map_err(|error| ToolError::Runtime {
            message: format!("screenshot redaction worker failed: {error}"),
        })?
        .map_err(ToolError::Computer)?;
        cancel.check().map_err(ToolError::Computer)?;
        let mut cas = self.cas.lock().await;
        cas.put_image(&redacted, "image/png").await
    }

    async fn record_computer_observation(
        &self,
        record: ComputerObservationRecord<'_>,
    ) -> ToolResult<()> {
        let ComputerObservationRecord {
            run_id,
            call_id,
            intent,
            target,
            observation,
            image,
            detail,
        } = record;
        let Some(target) = target else {
            return Ok(());
        };
        let request_json = serde_json::json!({
            "run_id": run_id,
            "call_id": call_id,
            "effect_id": intent.effect,
            "effect_args_digest": intent.args_digest,
            "graph_id": target.graph_id,
            "node": target.node,
            "attempt": target.attempt,
            "observation": observation,
            "image": image,
        })
        .to_string();
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let mut identity = blake3::Hasher::new();
        for part in [
            self.session_id.as_str(),
            run_id.as_str(),
            call_id,
            intent.effect.as_str(),
        ] {
            identity.update(&(part.len() as u64).to_be_bytes());
            identity.update(part.as_bytes());
        }
        let command = ComputerEvidenceCommand {
            command_id: format!("computer-evidence-{}", identity.finalize().to_hex()),
            request_digest,
            request_json,
            session_id: self.session_id.clone(),
            worker_generation: self.output.store.worker_generation(),
            run_id: run_id.clone(),
            call_id: call_id.to_owned(),
            effect_id: intent.effect.clone(),
            effect_args_digest: intent.args_digest.clone(),
            graph_id: target.graph_id,
            node: target.node,
            attempt: target.attempt,
            observation,
            image: image.clone(),
            detail,
            device_id: self.output.device_id.clone(),
        };
        match self
            .output
            .store
            .hub()
            .record_computer_evidence(command)
            .await
        {
            Ok(ComputerEvidenceOutcome::Committed { .. })
            | Ok(ComputerEvidenceOutcome::IdempotentReplay { .. })
            | Ok(ComputerEvidenceOutcome::StaleGraph) => Ok(()),
            Err(SessionHubError::Store(error)) => Err(ToolError::Runtime {
                message: error.message,
            }),
            Err(error) => Err(ToolError::Runtime {
                message: error.to_string(),
            }),
        }
    }

    async fn close_effects(&self, cancelled: bool) -> Result<(), HaiderError> {
        let pollers = self
            .permission_pollers
            .lock()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "computer permission poller lock is poisoned",
                    false,
                )
            })?
            .drain()
            .map(|(_, task)| task)
            .collect::<Vec<_>>();
        for task in pollers {
            task.abort();
        }
        let emergency_stop = self.computer.emergency_stop().await;
        let mut broker_guard = self.broker.lock().await;
        let computer_turn_cancelled = self
            .active_computer_turn_cancel
            .lock()
            .map_err(|_| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "computer cancellation state lock is poisoned",
                    false,
                )
            })?
            .as_ref()
            .is_some_and(CancelToken::is_cancelled);
        if (cancelled || computer_turn_cancelled)
            && let Some(broker) = broker_guard.as_mut()
        {
            broker.cancel_computer_actions();
            broker.cancel_mobile_actions();
        }
        let broker = broker_guard.take();
        drop(broker_guard);
        let Some(broker) = broker else {
            return emergency_stop.map_err(computer_error);
        };
        let closed = if cancelled || computer_turn_cancelled {
            broker.cancel().await
        } else {
            broker.close().await
        }
        .map(|_| ())
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::EffectUnknownOutcome,
                format!("effect broker close reported unfinished work: {error}"),
                false,
            )
        });
        closed?;
        emergency_stop.map_err(computer_error)
    }
}

#[async_trait]
impl ToolDispatcher for BrokerToolDispatcher {
    async fn refresh_volatile_context_tail(&self) -> Result<Option<String>, HaiderError> {
        if self.parent_agent_id.is_some()
            && self
                .grant
                .as_ref()
                .is_none_or(|grant| !grant.tools.iter().any(|tool| tool == "graph_evidence"))
        {
            return Ok(None);
        }
        let brief = self
            .output
            .store
            .hub()
            .graph_status(&self.session_id)
            .await
            .map_err(hub_error)?
            .and_then(|status| status.graph_brief())
            .unwrap_or_default();
        // An empty managed tail explicitly clears a brief after completion or
        // abandonment in the middle of the same provider tool loop.
        Ok(Some(brief))
    }

    #[allow(clippy::expect_used)]
    async fn execute(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        if cancel.is_cancelled() {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "tool dispatch was cancelled before start",
                false,
            ));
        }
        if name == "mobile" && !self.mobile_use_active {
            return Ok(ToolDispatchResult::Completed(
                mobile_capability_denied_result(),
            ));
        }
        if let Some(grant) = &self.grant {
            let allowed = grant.tools.iter().any(|allowed| allowed == name)
                && registered_tools()
                    .into_iter()
                    .find(|entry| entry.manifest.name == name)
                    .is_some_and(|entry| {
                        entry
                            .manifest
                            .effects
                            .iter()
                            .all(|effect| grant_admits_manifest_effect(grant, effect))
                    });
            if !allowed {
                return Ok(ToolDispatchResult::Completed(grant_ceiling_result(name)));
            }
        }
        let route = registered_tool_route(name).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("unsupported tool `{name}`"),
                false,
            )
        })?;
        if route == RegisteredToolRoute::GraphEvidence {
            let mut request = match GraphEvidence::from_tool_args(args) {
                Ok(request) => request,
                Err(error) => {
                    return Ok(ToolDispatchResult::Completed(graph_evidence_rejection(
                        ErrorCode::InvalidArgument,
                        &error.to_string(),
                        None,
                    )));
                }
            };
            if request.graph_id.is_none() {
                let status = match self.output.store.hub().graph_status(&self.session_id).await {
                    Ok(Some(status)) => status,
                    Ok(None) => {
                        return Ok(ToolDispatchResult::Completed(graph_evidence_rejection(
                            ErrorCode::GraphNotActive,
                            "session has no Convergence Graph",
                            None,
                        )));
                    }
                    Err(SessionHubError::Store(error)) => return Err(error),
                    Err(error) => {
                        return Err(HaiderError::new(
                            ErrorCode::Internal,
                            error.to_string(),
                            false,
                        ));
                    }
                };
                request.graph_id = Some(status.graph_id);
            }
            let request_json = serde_json::to_string(&request).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("cannot encode graph evidence request: {error}"),
                    false,
                )
            })?;
            let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
            let mut identity = blake3::Hasher::new();
            for part in [self.session_id.as_str(), run_id.as_str(), call_id] {
                identity.update(&(part.len() as u64).to_be_bytes());
                identity.update(part.as_bytes());
            }
            let command = GraphEvidenceCommand {
                command_id: format!("graph-evidence-{}", identity.finalize().to_hex()),
                request_digest,
                request_json,
                session_id: self.session_id.clone(),
                worker_generation: self.output.store.worker_generation(),
                run_id: run_id.clone(),
                call_id: call_id.to_owned(),
                graph_id: request
                    .graph_id
                    .clone()
                    .expect("graph evidence target is resolved above"),
                node: request.node,
                verdict: request.verdict,
                detail: request.detail,
                slot: request.slot,
                subject_digest: request.subject_digest,
                signal: request.signal,
                workspace_mutation: request.workspace_mutation,
                child_contract: None,
                device_id: self.output.device_id.clone(),
            };
            let recorded = match self.output.store.hub().record_graph_evidence(command).await {
                Ok(GraphEvidenceOutcome::Committed { recorded, .. })
                | Ok(GraphEvidenceOutcome::IdempotentReplay { recorded }) => recorded,
                Err(SessionHubError::Store(error))
                    if matches!(
                        error.code,
                        ErrorCode::GraphNotActive
                            | ErrorCode::GraphWrongNode
                            | ErrorCode::InvalidArgument
                            | ErrorCode::RevisionConflict
                    ) =>
                {
                    return Ok(ToolDispatchResult::Completed(graph_evidence_rejection(
                        error.code,
                        &error.message,
                        error
                            .details
                            .as_ref()
                            .and_then(|details| details.get("kind"))
                            .and_then(serde_json::Value::as_str),
                    )));
                }
                Err(SessionHubError::Store(error)) => return Err(error),
                Err(error) => {
                    return Err(HaiderError::new(
                        ErrorCode::Internal,
                        error.to_string(),
                        false,
                    ));
                }
            };
            let preview = serde_json::to_string(&serde_json::json!({
                "ok": true,
                "graph_id": recorded.graph_id,
                "node": recorded.node,
                "attempt": recorded.attempt,
                "fingerprint": recorded.fingerprint,
                "through_seq": recorded.through_seq,
            }))
            .unwrap_or_else(|_| "{\"ok\":true}".into());
            return Ok(ToolDispatchResult::Completed(BoundedResult {
                preview,
                truncated: false,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            }));
        }
        if route == RegisteredToolRoute::WorkflowAuthor {
            let authorized = self.parent_agent_id.is_some()
                && self
                    .grant
                    .as_ref()
                    .is_some_and(|grant| grant.tools.iter().any(|tool| tool == "workflow_author"));
            if !authorized {
                return Ok(ToolDispatchResult::Completed(grant_ceiling_result(name)));
            }
            let request = WorkflowAuthor::from_tool_args(args).map_err(tool_error)?;
            if request.template.nodes.iter().any(|node| {
                matches!(
                    node.gate,
                    haider_protocol::graph::GraphGateKind::HumanConfirm
                )
            }) {
                return Ok(ToolDispatchResult::Completed(BoundedResult {
                    preview: serde_json::json!({
                        "status": "rejected",
                        "kind": "child_human_gate_forbidden",
                    })
                    .to_string(),
                    truncated: false,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: ToolResultStatus::Rejected,
                    reason: Some("delegated workflows cannot contain a human-confirm gate".into()),
                    presentation: None,
                }));
            }
            let current = self
                .output
                .store
                .hub()
                .graph_status(&self.session_id)
                .await
                .map_err(hub_error)?
                .filter(haider_protocol::graph::GraphStatus::is_unfinished)
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::GraphNotActive,
                        "workflow_author requires this child's active graph",
                        false,
                    )
                })?;
            let request_json = serde_json::to_string(&request)
                .map_err(|error| HaiderError::new(ErrorCode::Internal, error.to_string(), false))?;
            let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
            let new_graph_id = GraphId::new(format!(
                "graph-authored-{}",
                crate::delegation::stable_digest(&[
                    self.session_id.as_str(),
                    run_id.as_str(),
                    call_id,
                ])
            ));
            let switched = self
                .output
                .store
                .hub()
                .switch_graph(GraphSwitchCommand {
                    command_id: format!("workflow-author-{new_graph_id}"),
                    request_digest,
                    request_json,
                    session_id: self.session_id.clone(),
                    worker_generation: self.output.store.worker_generation(),
                    old_graph_id: current.graph_id,
                    new_graph_id,
                    template: request.template.name.clone(),
                    template_spec: Some(request.template),
                    device_id: self.output.device_id.clone(),
                })
                .await
                .map_err(hub_error)?;
            let switched = match switched {
                GraphSwitchOutcome::Committed { switched, .. }
                | GraphSwitchOutcome::IdempotentReplay { switched } => switched,
            };
            return Ok(ToolDispatchResult::Completed(BoundedResult {
                preview: serde_json::json!({
                    "ok": true,
                    "graph_id": switched.new_graph_id,
                    "template": switched.template,
                    "digest": switched.digest,
                })
                .to_string(),
                truncated: false,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            }));
        }
        let mut broker_guard = self.broker.lock().await;
        let _active_tool = ActiveToolName::set(&self.active_tool_name, name).map_err(tool_error)?;
        let broker = broker_guard.as_mut().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "tool dispatcher is already closed",
                false,
            )
        })?;
        let policy = self.policy.lock().await;
        if route == RegisteredToolRoute::SpawnSubagent {
            if let Err(error) = self
                .delegation
                .validate_spawn_depth(&self.session_id, self.parent_agent_id.as_ref())
                .await
            {
                if error.code == ErrorCode::InvalidArgument
                    && error.message == crate::delegation::RECURSION_LIMIT_MESSAGE
                {
                    return Ok(ToolDispatchResult::Completed(recursion_limit_result()));
                }
                return Err(error);
            }
            let mut request = SpawnSubagent::from_tool_args(args).map_err(tool_error)?;
            // C2: a TYPED child. Resolve the agent type from the Loom registry
            // BEFORE any durable spawn work: the type's Job becomes the
            // child's role framing and the display label carries `@type ·` so
            // surfaces can color the chip. Unknown types are a completed
            // rejection the model can correct — never a turn failure.
            let mut typed_record: Option<haider_protocol::loom::LoomAgentType> = None;
            if let Some(type_id) = request.agent_type.clone() {
                let record = self.output.store.hub().loom_agent_type(&type_id).await?;
                let Some(record) = record else {
                    return Ok(ToolDispatchResult::Completed(BoundedResult {
                        preview: serde_json::json!({
                            "ok": false,
                            "error": format!(
                                "unknown agent type `{type_id}` — register it in the Loom registry or drop agent_type"
                            ),
                        })
                        .to_string(),
                        truncated: false,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: ToolResultStatus::Completed,
                        reason: None,
                        presentation: None,
                    }));
                };
                typed_record = Some(record.clone());
                // Verify-fix C5: registry strings frame a SINGLE-LINE role
                // header — newlines must not fake a second header.
                let line = |value: &str| value.replace(['\n', '\r'], " ");
                request.prompt = format!(
                    "[agent type @{} — {} · {} -> {}]\n{}\n\n{}",
                    record.id,
                    line(&record.name),
                    line(&record.in_type),
                    line(&record.out_type),
                    record.job,
                    request.prompt
                );
                // Verify-fix C3: the `@type ·` chip convention is DAEMON
                // truth, never model input — a task that already leads with
                // `@` is stripped before the honest prefix goes on.
                let clean = request.task.trim_start_matches('@').trim_start().to_owned();
                request.task = format!("@{} · {}", record.id, clean);
            } else if request.task.starts_with('@') {
                // An UNTYPED spawn must not cosplay as a specialist.
                request.task = request.task.trim_start_matches('@').trim_start().to_owned();
            }
            // F1: resolve the child's pair BEFORE any durable spawn work. A
            // typed selection refusal is a completed tool result — the model
            // retries with an explicit pair — never a turn failure.
            let child_metadata = match self
                .delegation
                .resolve_child_metadata(&self.metadata, &request)?
            {
                Ok(metadata) => metadata,
                Err(refusal) => {
                    return Ok(ToolDispatchResult::Completed(selection_rejection_result(
                        &refusal,
                    )));
                }
            };
            let intent = match broker.begin_agent_spawn(&request, &policy).await {
                Ok(intent) => intent,
                Err(ToolError::AuthorizationRequired { menu }) => {
                    let menu = broker.permission_menu(&menu).cloned().ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::Internal,
                            "broker authorization menu disappeared before publication",
                            false,
                        )
                    })?;
                    return Ok(ToolDispatchResult::ApprovalRequired(menu));
                }
                Err(error) => return Err(tool_error(error)),
            };
            let established = match self
                .delegation
                .establish(
                    SpawnCoordinates {
                        parent_session_id: self.session_id.clone(),
                        parent_run_id: run_id.clone(),
                        parent_branch_id: self.branch_id.clone(),
                        parent_agent_id: self.parent_agent_id.clone(),
                        tool_item_id: item_id.clone(),
                        call_id: call_id.to_owned(),
                        metadata: child_metadata,
                        agent_type: typed_record,
                    },
                    request,
                )
                .await
            {
                Ok(established) => established,
                Err(error) => {
                    let spawn_error = ToolError::Runtime {
                        message: error.message.clone(),
                    };
                    if error.presentation.as_ref().is_some_and(|presentation| {
                        presentation.subcode.as_str() == "subagent-limit-reached"
                    }) {
                        // The broker's `finish` helper deliberately passes a
                        // failed result back to its caller. For this admitted,
                        // continuable tool rejection, journal the identical
                        // failed effect outcome directly and keep the typed
                        // result on the model's tool loop.
                        broker
                            .journal_outcome(
                                &intent,
                                EffectOutcome::Failed {
                                    error: spawn_error.to_string(),
                                },
                            )
                            .await
                            .map_err(tool_error)?;
                        return Ok(ToolDispatchResult::Completed(subagent_limit_result(&error)));
                    }
                    broker
                        .finish_agent_spawn(&intent, Err(spawn_error))
                        .await
                        .map_err(tool_error)?;
                    return Err(error);
                }
            };
            // The durable effect ends at establishment, before the child can
            // start provider work. The manager submission below is therefore
            // outside the broker effect lifetime.
            broker
                .finish_agent_spawn(&intent, Ok(()))
                .await
                .map_err(tool_error)?;
            drop(policy);
            drop(broker_guard);
            if let Err(error) = self.delegation.launch(&established).await {
                self.delegation
                    .record_launch_failure(&established.ticket, &error)
                    .await?;
            }
            self.deferred.lock().await.insert(
                established.ticket.manifest.agent.clone(),
                established.ticket.clone(),
            );
            return Ok(ToolDispatchResult::Deferred(established.ticket));
        }
        if route == RegisteredToolRoute::MessageSubagent {
            let request = MessageSubagent::from_tool_args(args).map_err(tool_error)?;
            let agent = request.agent.clone();
            let receipt = match self
                .delegation
                .message(
                    MessageCoordinates {
                        parent_session_id: self.session_id.clone(),
                        parent_agent_id: self.parent_agent_id.clone(),
                        command_id: format!("tool-{run_id}-{call_id}"),
                    },
                    request,
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(error)
                    if error.code == ErrorCode::InvalidArgument
                        && error.details.as_ref().is_some_and(|details| {
                            details.get("kind").and_then(serde_json::Value::as_str)
                                == Some("not_owned_child")
                        }) =>
                {
                    return Ok(ToolDispatchResult::Completed(BoundedResult {
                        preview: serde_json::json!({
                            "status": "rejected",
                            "kind": "not_owned_child",
                            "agent": agent,
                            "message": error.message,
                        })
                        .to_string(),
                        truncated: false,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: ToolResultStatus::Rejected,
                        reason: Some(error.message.clone()),
                        presentation: None,
                    }));
                }
                Err(error) => return Err(error),
            };
            let preview = serde_json::to_string(&receipt).map_err(|error| {
                HaiderError::new(
                    ErrorCode::Internal,
                    format!("cannot encode message_subagent receipt: {error}"),
                    false,
                )
            })?;
            return Ok(ToolDispatchResult::Completed(BoundedResult {
                preview,
                truncated: false,
                artifact: None,
                images: Vec::new(),
                cursor: None,
                status: ToolResultStatus::Completed,
                reason: None,
                presentation: None,
            }));
        }
        let fs_write_count = self
            .ledger
            .changes_for(&self.session_id, run_id)
            .map_or(0, |changes| changes.writes.len());
        let result = match route {
            RegisteredToolRoute::FsRead => {
                let path = required_string(&args, "path")?;
                let offset = optional_u64(&args, "offset")?
                    .map(usize::try_from)
                    .transpose()
                    .map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "tool argument `offset` is too large",
                            false,
                        )
                    })?;
                let limit = optional_u64(&args, "limit")?
                    .map(usize::try_from)
                    .transpose()
                    .map_err(|_| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "tool argument `limit` is too large",
                            false,
                        )
                    })?;
                let mut cas = self.cas.lock().await;
                broker
                    .fs_read(
                        &FsRead::new(path).with_line_range(offset, limit),
                        &policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            RegisteredToolRoute::FsSearch => {
                let root = optional_string(&args, "path")?
                    .or(optional_string(&args, "root")?)
                    .unwrap_or_else(|| ".".into());
                let query = optional_string(&args, "pattern")?
                    .or(optional_string(&args, "query")?)
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "tool argument `pattern` must be a non-empty string",
                            false,
                        )
                    })?;
                let glob = optional_string(&args, "glob")?;
                let case_mode = match optional_string(&args, "case")?.as_deref() {
                    None | Some("sensitive") => FsCaseMode::Sensitive,
                    Some("insensitive") => FsCaseMode::Insensitive,
                    Some("smart") => FsCaseMode::Smart,
                    Some(value) => {
                        return Err(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("unsupported fs_search case mode `{value}`"),
                            false,
                        ));
                    }
                };
                let mode = match optional_string(&args, "mode")?.as_deref() {
                    None | Some("literal") => FsSearchMode::Literal,
                    Some("simple") => FsSearchMode::Simple,
                    Some(value) => {
                        return Err(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("unsupported fs_search pattern mode `{value}`"),
                            false,
                        ));
                    }
                };
                let mut operation = FsSearch::new(root, query)
                    .with_case_mode(case_mode)
                    .with_mode(mode);
                if let Some(glob) = glob {
                    operation = operation.with_glob(glob);
                }
                let mut cas = self.cas.lock().await;
                broker
                    .fs_search(&operation, &policy, &mut *cas, ResultBounds::default())
                    .await
            }
            RegisteredToolRoute::FsGlob => {
                let root = optional_string(&args, "path")?.unwrap_or_else(|| ".".into());
                let pattern = required_string(&args, "pattern")?;
                let mut cas = self.cas.lock().await;
                broker
                    .fs_glob(
                        &FsGlob::new(root, pattern),
                        &policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            RegisteredToolRoute::ProcessExec => {
                let command = required_string(&args, "command")?;
                // B3 (review round 2) — the typed CLI fence: a leaf
                // specialist runs its DECLARED CLIs, one program per call
                // (foreground AND background). A refusal is a completed
                // typed result the model can react to.
                if let Some(scope) = &self.cli_scope
                    && let Err(reason) = cli_scope_admits(scope, &command)
                {
                    return Ok(ToolDispatchResult::Completed(BoundedResult {
                        preview: serde_json::json!({ "ok": false, "error": reason }).to_string(),
                        truncated: false,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: ToolResultStatus::Completed,
                        reason: None,
                        presentation: None,
                    }));
                }
                let cwd = optional_string(&args, "cwd")?;
                let background = optional_bool(&args, "background")?.unwrap_or(false);
                if background {
                    // Image discovery is foreground-only: detached tasks have
                    // no completed bounded transcript at this dispatch seam.
                    // W-A: the detached shape returns IMMEDIATELY with the
                    // typed running receipt; supervision is session-scoped.
                    let name = optional_string(&args, "name")?;
                    self.tasks
                        .spawn_background(
                            crate::tasks::TaskSpawnContext {
                                session_id: self.session_id.clone(),
                                run_id: run_id.clone(),
                                branch_id: self.branch_id.clone(),
                                agent_id: self.parent_agent_id.clone(),
                                call_id: call_id.to_owned(),
                            },
                            command,
                            cwd,
                            name,
                            broker,
                            &policy,
                        )
                        .await
                } else {
                    let effective_cwd = command_cwd(&self.metadata.cwd, cwd.as_deref());
                    let mut operation = ProcessExec::new(call_id, command.clone());
                    if let Some(cwd) = cwd {
                        operation = operation.with_cwd(cwd);
                    }
                    let cas = self.cas.lock().await.clone();
                    let output = self.output.sink(
                        run_id.clone(),
                        item_id.clone(),
                        call_id.to_owned(),
                        PromptRender::Omit,
                    );
                    let started = SystemTime::now();
                    match broker
                        .process_exec(&operation, &policy, cas, output, ProcessBounds::default())
                        .await
                    {
                        Ok(execution) => match execution.wait().await {
                            Ok(result) => {
                                match self.output.record_process_signal(run_id, &result).await {
                                    Ok(signal) => {
                                        if result.status
                                            == haider_protocol::item::ToolStatus::Completed
                                        {
                                            let preview = process_output_preview(&result);
                                            if let Err(error) = self
                                                .emit_created_images(CreatedImageScan {
                                                    run_id,
                                                    call_id,
                                                    tool: "process_exec",
                                                    command: &command,
                                                    output_preview: &preview,
                                                    cwd: &effective_cwd,
                                                    started,
                                                })
                                                .await
                                            {
                                                return Err(tool_error(error));
                                            }
                                        }
                                        Ok(process_result_with_signal(result, Some(&signal)))
                                    }
                                    Err(error) => Err(ToolError::Runtime {
                                        message: error.message,
                                    }),
                                }
                            }
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    }
                }
            }
            RegisteredToolRoute::LoomRegister => {
                let kind = required_string(&args, "kind")?;
                let completed = |value: serde_json::Value| BoundedResult {
                    preview: value.to_string(),
                    truncated: false,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: ToolResultStatus::Completed,
                    reason: None,
                    presentation: None,
                };
                let refusal = |error: String| {
                    completed(serde_json::json!({ "ok": false, "error": error }))
                };
                let receipt = |kind: &str,
                               registration: haider_protocol::loom::LoomRegistration| {
                    completed(serde_json::json!({
                        "ok": true,
                        "kind": kind,
                        "id": registration.id,
                        "rev": registration.rev,
                        "digest": registration.digest,
                        "updated": registration.updated,
                    }))
                };
                let bodies =
                    accepted_plan_bodies(&self.output.store, self.branch_id.as_ref()).await?;
                match kind.as_str() {
                    "workflow" => {
                        let source = required_string(&args, "source")?;
                        if plan_gate_admits(&bodies, &[source.trim()]) {
                            match self.output.store.hub().loom_register_workflow(source).await {
                                Ok(registration) => Ok(receipt("workflow", registration)),
                                Err(error) if error.code == ErrorCode::InvalidArgument => {
                                    Ok(refusal(format!(
                                        "registration rejected: {}",
                                        error.message
                                    )))
                                }
                                Err(error) => Err(ToolError::Runtime {
                                    message: error.message,
                                }),
                            }
                        } else {
                            Ok(refusal(
                                "registration requires a plan the human ACCEPTED whose body \
                                 contains this exact pipe source — present one with the `plan` \
                                 tool first"
                                    .into(),
                            ))
                        }
                    }
                    "agent_type" => {
                        let mut value = args.get("record").cloned().ok_or_else(|| {
                            HaiderError::new(
                                ErrorCode::InvalidArgument,
                                "kind=agent_type requires `record`",
                                false,
                            )
                        })?;
                        if let Some(object) = value.as_object_mut() {
                            // The registry owns revs; the model never picks one.
                            object.insert("rev".into(), serde_json::json!(1));
                        }
                        let record: haider_protocol::loom::LoomAgentType =
                            serde_json::from_value(value).map_err(|error| {
                                HaiderError::new(
                                    ErrorCode::InvalidArgument,
                                    format!("agent type record does not decode: {error}"),
                                    false,
                                )
                            })?;
                        let signature = format!("{} -> {}", record.in_type, record.out_type);
                        let needles = [record.id.as_str(), record.job.trim(), signature.as_str()];
                        if plan_gate_admits(&bodies, &needles) {
                            match self
                                .output
                                .store
                                .hub()
                                .loom_register_agent_type(record)
                                .await
                            {
                                Ok(registration) => Ok(receipt("agent_type", registration)),
                                Err(error) if error.code == ErrorCode::InvalidArgument => {
                                    Ok(refusal(format!(
                                        "registration rejected: {}",
                                        error.message
                                    )))
                                }
                                Err(error) => Err(ToolError::Runtime {
                                    message: error.message,
                                }),
                            }
                        } else {
                            Ok(refusal(
                                "registration requires a plan the human ACCEPTED whose body \
                                 contains the type id, its job text, and its `In -> Out` \
                                 signature — present one with the `plan` tool first"
                                    .into(),
                            ))
                        }
                    }
                    other => {
                        return Err(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("loom_register kind `{other}` is not workflow|agent_type"),
                            false,
                        ));
                    }
                }
            }
            RegisteredToolRoute::TaskOutput => {
                let task_id = required_string(&args, "task_id")?;
                let cursor = optional_u64(&args, "cursor")?;
                self.tasks
                    .task_output(&self.session_id, &task_id, cursor)
                    .await
            }
            RegisteredToolRoute::TaskKill => {
                let task_id = required_string(&args, "task_id")?;
                self.tasks
                    .task_kill(&self.session_id, &task_id, broker, &policy)
                    .await
            }
            RegisteredToolRoute::WebFetch => {
                // W-B (LW7): intent → authorization → dispatch journal
                // through the broker, the guarded engine in between, and an
                // HONEST terminal outcome either way. A failed fetch is a
                // typed tool RESULT the model can react to, never a turn
                // failure; only journal-append failures abort.
                let operation = WebFetch::from_tool_args(&args).map_err(tool_error)?;
                // B2 — the use-site host fence: a typed child's grant scopes
                // the network to its DECLARED APIs; a fetch outside them is a
                // completed refusal the model can react to.
                if let Some(grant) = &self.grant
                    && !web_fetch_host_allowed(grant, operation.host())
                {
                    return Ok(ToolDispatchResult::Completed(BoundedResult {
                        preview: serde_json::json!({
                            "ok": false,
                            "error": format!(
                                "host `{}` is outside this agent's granted APIs",
                                operation.host()
                            ),
                        })
                        .to_string(),
                        truncated: false,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: ToolResultStatus::Completed,
                        reason: None,
                        presentation: None,
                    }));
                }
                match broker.begin_web_fetch(&operation, &policy).await {
                    Ok(intent) => {
                        let execution = tokio::select! {
                            () = cancel.cancelled() => {
                                broker
                                    .journal_outcome(&intent, EffectOutcome::Cancelled)
                                    .await
                                    .map_err(tool_error)?;
                                return Err(HaiderError::new(
                                    ErrorCode::Internal,
                                    "web_fetch was cancelled mid-flight",
                                    false,
                                ));
                            }
                            fetched = async {
                                // B2 (review round 2) — the host fence above
                                // checked hop 0; a typed grant's scope must
                                // also hold across every REDIRECT hop, so the
                                // scoped engine re-checks per hop.
                                match self.grant.as_ref().and_then(scoped_network_hosts) {
                                    Some(hosts) => {
                                        haider_provider::fetch_public_url_scoped_with_one_retry(
                                            operation.url(),
                                            operation.max_bytes(),
                                            &hosts,
                                        )
                                        .await
                                    }
                                    None => haider_provider::fetch_public_url_with_one_retry(
                                        operation.url(),
                                        operation.max_bytes(),
                                    )
                                    .await,
                                }
                            } => fetched,
                        };
                        let retried = execution.attempts == 2;
                        let fetched = execution.outcome;
                        match fetched {
                            Ok(outcome) => {
                                broker
                                    .journal_outcome(&intent, EffectOutcome::Ok)
                                    .await
                                    .map_err(tool_error)?;
                                Ok(BoundedResult {
                                    preview: format!(
                                        "[{} · {}]\n{}",
                                        outcome.final_url, outcome.content_type, outcome.text
                                    ),
                                    truncated: outcome.truncated,
                                    artifact: None,
                                    images: Vec::new(),
                                    cursor: None,
                                    status: ToolResultStatus::Completed,
                                    reason: retried.then(|| {
                                        "transient web_fetch failure — retry 2/2 succeeded".into()
                                    }),
                                    presentation: None,
                                })
                            }
                            Err(error) => {
                                let message = error.to_string();
                                broker
                                    .journal_outcome(
                                        &intent,
                                        EffectOutcome::Failed {
                                            error: message.clone(),
                                        },
                                    )
                                    .await
                                    .map_err(tool_error)?;
                                Ok(BoundedResult {
                                    preview: format!("web_fetch failed: {message}"),
                                    truncated: false,
                                    artifact: None,
                                    images: Vec::new(),
                                    cursor: None,
                                    status: ToolResultStatus::Failed,
                                    reason: Some(if retried {
                                        format!(
                                            "retry 2/2 exhausted — {}",
                                            crate::worker::bounded_failure_reason(&message)
                                        )
                                    } else {
                                        crate::worker::bounded_failure_reason(&message)
                                    }),
                                    presentation: Some(if retried {
                                        ErrorPresentation::new(
                                            "web-fetch-retry-exhausted",
                                            "Web fetch failed after retry",
                                            "The idempotent GET was attempted twice and still failed.",
                                            ErrorScope::Tool,
                                            [ErrorAction::Retry],
                                        )
                                    } else {
                                        error.presentation
                                    }),
                                })
                            }
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            RegisteredToolRoute::FsWrite => {
                let path = required_string(&args, "path")?;
                let content = required_string_allow_empty(&args, "content")?;
                let attribution = TurnAttribution::new(self.session_id.clone(), run_id.clone());
                let started = SystemTime::now();
                let result = broker
                    .fs_write(
                        &FsWrite::new(path.clone(), content),
                        &policy,
                        &attribution,
                        &self.ledger,
                    )
                    .await;
                if result
                    .as_ref()
                    .is_ok_and(|result| result.status == ToolResultStatus::Completed)
                    && let Err(error) = self
                        .emit_created_images(CreatedImageScan {
                            run_id,
                            call_id,
                            tool: "fs_write",
                            // Verify round 2: the EXACT argument, quoted, so
                            // a path with spaces nominates whole instead of
                            // fragmenting into whitespace tokens.
                            command: &format!("\"{path}\""),
                            output_preview: "",
                            cwd: Path::new(&self.metadata.cwd),
                            started,
                        })
                        .await
                {
                    return Err(tool_error(error));
                }
                result
            }
            RegisteredToolRoute::FsEdit => {
                let path = required_string(&args, "path")?;
                let requested_edits = args
                    .get("edits")
                    .and_then(serde_json::Value::as_array)
                    .filter(|edits| !edits.is_empty())
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::InvalidArgument,
                            "tool argument `edits` must be a non-empty array",
                            false,
                        )
                    })?;
                let mut edits = Vec::with_capacity(requested_edits.len());
                for edit in requested_edits {
                    let old = required_string(edit, "old")?;
                    let new = required_string_allow_empty(edit, "new")?;
                    let replace_all = optional_bool(edit, "replace_all")?.unwrap_or(false);
                    edits.push(FsEditChange::new(old, new).replace_all(replace_all));
                }
                let attribution = TurnAttribution::new(self.session_id.clone(), run_id.clone());
                broker
                    .fs_edit(
                        &FsEdit::many(path, edits),
                        &policy,
                        &attribution,
                        &self.ledger,
                    )
                    .await
            }
            RegisteredToolRoute::FsPath => {
                let source = required_string(&args, "source")?;
                let operation = match required_string(&args, "operation")?.as_str() {
                    "move" => FsPathOperation::Move,
                    "delete" => FsPathOperation::Delete,
                    "copy" => FsPathOperation::Copy,
                    value => {
                        return Err(HaiderError::new(
                            ErrorCode::InvalidArgument,
                            format!("unsupported fs_path operation `{value}`"),
                            false,
                        ));
                    }
                };
                let destination = optional_string(&args, "destination")?;
                let overwrite = optional_bool(&args, "overwrite")?.unwrap_or(false);
                let mut operation = FsPath::new(operation, source).overwrite(overwrite);
                if let Some(destination) = destination {
                    operation = operation.with_destination(destination);
                }
                let attribution = TurnAttribution::new(self.session_id.clone(), run_id.clone());
                broker
                    .fs_path(&operation, &policy, &attribution, &self.ledger)
                    .await
            }
            RegisteredToolRoute::WebSearch => {
                // W-B (decision 3): the search rides the SAME subscription
                // credential surface as the turn itself — no broker, no
                // menu (the request_input pattern). Failures are typed tool
                // RESULTS; a 404/410 latches the session degrade so the
                // tool stops advertising next turn (no retry storm).
                let query = required_string(&args, "query")?;
                match &self.web_search {
                    None => Ok(BoundedResult {
                        preview:
                            "web_search is unavailable: no subscription search executor is configured"
                                .into(),
                        truncated: false,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: ToolResultStatus::Failed,
                        reason: Some("web_search is unavailable in this session".into()),
                        presentation: None,
                    }),
                    Some(executor) => {
                        match executor
                            .search(&self.metadata.model, self.session_id.as_str(), &query)
                            .await
                        {
                            Ok(text) => {
                                let (preview, truncated) = bounded_search_preview(text);
                                Ok(BoundedResult {
                                    preview,
                                    truncated,
                                    artifact: None,
                                    images: Vec::new(),
                                    cursor: None,
                                    status: ToolResultStatus::Completed,
                                    reason: None,
                                    presentation: None,
                                })
                            }
                            Err(failure) => {
                                if failure.degraded {
                                    self.output
                                        .store
                                        .hub()
                                        .degrade_openai_alpha_search(&self.session_id);
                                }
                                Ok(BoundedResult {
                                    preview: format!("web_search failed: {}", failure.message),
                                    truncated: false,
                                    artifact: None,
                                    images: Vec::new(),
                                    cursor: None,
                                    status: ToolResultStatus::Failed,
                                    reason: Some(bounded_failure_reason(&failure.message)),
                                    presentation: None,
                                })
                            }
                        }
                    }
                }
            }
            RegisteredToolRoute::Computer => {
                async {
                    let operation = ComputerOperation::from_tool_args(args)?;
                    let action_cancel = ComputerCancelToken::new();
                    let intent = broker
                        .authorize_computer(&operation, &policy)
                        .await?;
                    match self
                        .computer
                        .prepare(operation.action(), &action_cancel)
                        .await
                    {
                        Ok(()) => {
                            broker
                                .dispatch_computer(&intent, action_cancel.clone())
                                .await?;
                        }
                        Err(error @ ComputerError::PermissionRequired { .. }) => {
                            let pending = Self::pending_permission(&intent, &error)
                                .expect("permission error has a grant descriptor");
                            let menu = Self::permission_menu(
                                &intent,
                                pending.permission,
                                &pending.pane_name,
                            );
                            self.pending_computer_permissions
                                .lock()
                                .await
                                .insert(menu.id.clone(), pending);
                            self.os_permission_menus
                                .lock()
                                .await
                                .insert(menu.id.clone(), menu.clone());
                            return Err(ToolError::AuthorizationRequired { menu: menu.id });
                        }
                        Err(error) => return Err(ToolError::Computer(error)),
                    }
                    *self
                        .active_computer_turn_cancel
                        .lock()
                        .map_err(|_| ToolError::Runtime {
                            message: "computer cancellation state lock is poisoned".into(),
                        })? = Some(cancel.clone());
                    let graph_target = if matches!(
                        operation.action(),
                        haider_protocol::computer::ComputerAction::Screenshot
                            | haider_protocol::computer::ComputerAction::Inspect { .. }
                    ) {
                        match self.computer_graph_target().await {
                            Ok(target) => target,
                            Err(error) => {
                                broker
                                    .journal_computer_outcome(
                                        &intent,
                                        EffectOutcome::Failed {
                                            error: error.to_string(),
                                        },
                                    )
                                    .await?;
                                return Err(error);
                            }
                        }
                    } else {
                        None
                    };
                    match self.computer.execute(operation.action(), &action_cancel).await {
                        Ok(ComputerOutput::ScreenshotPng(png)) => {
                            let stored = self
                                .admit_computer_screenshot(png, &action_cancel)
                                .await;
                            match stored {
                                Ok(image) => {
                                    if let Err(error) =
                                        self.computer.set_viewport(image.width, image.height)
                                    {
                                        let tool_error = ToolError::Computer(error.clone());
                                        broker
                                            .journal_computer_outcome(
                                                &intent,
                                                EffectOutcome::Failed {
                                                    error: tool_error.to_string(),
                                                },
                                            )
                                            .await?;
                                        Ok(computer_failure_result(&error))
                                    } else {
                                        broker
                                            .journal_computer_outcome(&intent, EffectOutcome::Ok)
                                            .await?;
                                        self.record_computer_observation(
                                            ComputerObservationRecord {
                                                run_id,
                                                call_id,
                                                intent: &intent,
                                                target: graph_target,
                                                observation: ComputerObservationKind::Screenshot,
                                                image: &image,
                                                detail: format!(
                                                "computer screenshot captured ({}x{})",
                                                image.width, image.height
                                                ),
                                            },
                                        )
                                        .await?;
                                        Ok(BoundedResult {
                                            preview: format!(
                                                "screenshot captured ({}x{})",
                                                image.width, image.height
                                            ),
                                            truncated: false,
                                            artifact: None,
                                            images: vec![image],
                                            cursor: None,
                                            status: ToolResultStatus::Completed,
                                            reason: None,
                                            presentation: None,
                                        })
                                    }
                                }
                                Err(error) => {
                                    broker
                                        .journal_computer_outcome(
                                            &intent,
                                            EffectOutcome::Failed {
                                                error: error.to_string(),
                                            },
                                        )
                                        .await?;
                                    Err(error)
                                }
                            }
                        }
                        Ok(ComputerOutput::CursorPosition { x, y }) => {
                            broker
                                .journal_computer_outcome(&intent, EffectOutcome::Ok)
                                .await?;
                            Ok(BoundedResult {
                                preview: serde_json::json!({"x": x, "y": y}).to_string(),
                                truncated: false,
                                artifact: None,
                                images: Vec::new(),
                                cursor: None,
                                status: ToolResultStatus::Completed,
                                reason: None,
                                presentation: None,
                            })
                        }
                        Ok(ComputerOutput::Inspection {
                            inspection,
                            screenshot_png,
                        }) => {
                            let stored = self
                                .admit_computer_screenshot(screenshot_png, &action_cancel)
                                .await;
                            match stored {
                                Ok(image) => {
                                    if let Err(error) =
                                        self.computer.set_viewport(image.width, image.height)
                                    {
                                        let tool_error = ToolError::Computer(error.clone());
                                        broker
                                            .journal_computer_outcome(
                                                &intent,
                                                EffectOutcome::Failed {
                                                    error: tool_error.to_string(),
                                                },
                                            )
                                            .await?;
                                        Ok(computer_failure_result(&error))
                                    } else {
                                        let preview = match serde_json::to_string(&inspection) {
                                            Ok(preview) => preview,
                                            Err(error) => {
                                                let error = ToolError::Runtime {
                                                message: format!(
                                                    "could not encode computer accessibility inspection: {error}"
                                                ),
                                                };
                                                broker
                                                    .journal_computer_outcome(
                                                        &intent,
                                                        EffectOutcome::Failed {
                                                            error: error.to_string(),
                                                        },
                                                    )
                                                    .await?;
                                                return Err(error);
                                            }
                                        };
                                        broker
                                            .journal_computer_outcome(&intent, EffectOutcome::Ok)
                                            .await?;
                                        self.record_computer_observation(
                                            ComputerObservationRecord {
                                                run_id,
                                                call_id,
                                                intent: &intent,
                                                target: graph_target,
                                                observation: ComputerObservationKind::Inspect,
                                                image: &image,
                                                detail: format!(
                                                "computer inspection captured with screenshot ({}x{})",
                                                image.width, image.height
                                                ),
                                            },
                                        )
                                        .await?;
                                        Ok(BoundedResult {
                                            preview,
                                            truncated: false,
                                            artifact: None,
                                            images: vec![image],
                                            cursor: None,
                                            status: ToolResultStatus::Completed,
                                            reason: None,
                                            presentation: None,
                                        })
                                    }
                                }
                                Err(error) => {
                                    broker
                                        .journal_computer_outcome(
                                            &intent,
                                            EffectOutcome::Failed {
                                                error: error.to_string(),
                                            },
                                        )
                                        .await?;
                                    Err(error)
                                }
                            }
                        }
                        Ok(ComputerOutput::Confirmed { action }) => {
                            broker
                                .journal_computer_outcome(&intent, EffectOutcome::Ok)
                                .await?;
                            Ok(BoundedResult {
                                preview: format!("{action} completed"),
                                truncated: false,
                                artifact: None,
                                images: Vec::new(),
                                cursor: None,
                                status: ToolResultStatus::Completed,
                                reason: None,
                                presentation: None,
                            })
                        }
                        Err(ComputerError::Cancelled) => {
                            broker
                                .journal_computer_outcome(&intent, EffectOutcome::Cancelled)
                                .await?;
                            Err(ToolError::Computer(ComputerError::Cancelled))
                        }
                        Err(error) => {
                            broker
                                .journal_computer_outcome(
                                    &intent,
                                    EffectOutcome::Failed {
                                        error: error.to_string(),
                                    },
                                )
                                .await?;
                            Ok(computer_failure_result(&error))
                        }
                    }
                }
                .await
            }
            RegisteredToolRoute::Mobile => {
                async {
                    let operation = MobileOperation::from_tool_args(args)?;
                    let action_cancel = MobileCancelToken::new();
                    let intent = broker.authorize_mobile(&operation, &policy).await?;
                    self.mobile
                        .prepare(operation.action(), &action_cancel)
                        .await
                        .map_err(ToolError::Mobile)?;
                    broker
                        .dispatch_mobile(&intent, action_cancel.clone())
                        .await?;
                    match self.mobile.execute(operation.action(), &action_cancel).await {
                        Ok(output) => {
                            let preview = match serde_json::to_string(&output) {
                                Ok(preview) => preview,
                                Err(error) => {
                                    let error = ToolError::Runtime {
                                        message: format!(
                                            "could not encode mobile action output: {error}"
                                        ),
                                    };
                                    broker
                                        .journal_mobile_outcome(
                                            &intent,
                                            EffectOutcome::Failed {
                                                error: error.to_string(),
                                            },
                                        )
                                        .await?;
                                    return Err(error);
                                }
                            };
                            broker
                                .journal_mobile_outcome(&intent, EffectOutcome::Ok)
                                .await?;
                            Ok(BoundedResult {
                                preview,
                                truncated: false,
                                artifact: None,
                                images: Vec::new(),
                                cursor: None,
                                status: ToolResultStatus::Completed,
                                reason: None,
                                presentation: None,
                            })
                        }
                        Err(MobileError::Cancelled) => {
                            broker
                                .journal_mobile_outcome(&intent, EffectOutcome::Cancelled)
                                .await?;
                            Err(ToolError::Mobile(MobileError::Cancelled))
                        }
                        Err(error) => {
                            broker
                                .journal_mobile_outcome(
                                    &intent,
                                    EffectOutcome::Failed {
                                        error: error.to_string(),
                                    },
                                )
                                .await?;
                            Ok(mobile_failure_result(&error))
                        }
                    }
                }
                .await
            }
            RegisteredToolRoute::RequestInput
            | RegisteredToolRoute::Plan
            | RegisteredToolRoute::TodoWrite
            | RegisteredToolRoute::GraphEvidence
            | RegisteredToolRoute::WorkflowAuthor
            | RegisteredToolRoute::SpawnSubagent
            | RegisteredToolRoute::MessageSubagent => {
                return Err(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("tool `{name}` is not dispatched by the general-tool match"),
                    false,
                ));
            }
        };
        let result = if matches!(
            route,
            RegisteredToolRoute::FsWrite
                | RegisteredToolRoute::FsEdit
                | RegisteredToolRoute::FsPath
        ) {
            match result {
                Ok(mut result) => {
                    let Some(changes) = self.ledger.changes_for(&self.session_id, run_id) else {
                        return Err(HaiderError::new(
                            ErrorCode::Internal,
                            "filesystem mutation committed without ledger provenance",
                            false,
                        ));
                    };
                    let Some(record) = changes.writes.get(fs_write_count) else {
                        return Err(HaiderError::new(
                            ErrorCode::Internal,
                            "filesystem mutation committed without one new ledger record",
                            false,
                        ));
                    };
                    if changes.writes.len() != fs_write_count.saturating_add(1) {
                        return Err(HaiderError::new(
                            ErrorCode::Internal,
                            "filesystem mutation produced ambiguous ledger provenance",
                            false,
                        ));
                    }
                    let mutation =
                        durable_workspace_mutation(&self.output.store, run_id, &record.effect)
                            .await
                            .map_err(tool_error)?;
                    let subject_digest = mutation.subject_digest.clone();
                    let workspace_revision = mutation.workspace_revision.clone();
                    result.preview = serde_json::json!({
                        "result": result.preview,
                        "mutation_digest": mutation.mutation_digest,
                        "workspace_revision": workspace_revision,
                        "subject_digest": subject_digest,
                        "workspace_mutation": WorkspaceMutationRef {
                            run_id: run_id.clone(),
                            effect_id: record.effect.clone(),
                        },
                    })
                    .to_string();
                    Ok(result)
                }
                Err(error) => Err(error),
            }
        } else {
            result
        };
        match result {
            Ok(result) => Ok(ToolDispatchResult::Completed(result)),
            Err(haider_tools::ToolError::AuthorizationRequired { menu }) => {
                let menu = match broker.permission_menu(&menu).cloned() {
                    Some(menu) => menu,
                    None => self
                        .os_permission_menus
                        .lock()
                        .await
                        .get(&menu)
                        .cloned()
                        .ok_or_else(|| {
                            HaiderError::new(
                                ErrorCode::Internal,
                                "authorization menu disappeared before publication",
                                false,
                            )
                        })?,
                };
                Ok(ToolDispatchResult::ApprovalRequired(menu))
            }
            Err(error) => match typed_tool_result(&error) {
                Some(result) => Ok(ToolDispatchResult::Completed(result)),
                None => Err(tool_error(error)),
            },
        }
    }

    async fn collect_deferred(
        &self,
        ticket: &DeferredTicket,
        cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        self.delegation.collect(ticket, cancel).await
    }

    async fn acknowledge_deferred(&self, ticket: &DeferredTicket) -> Result<(), HaiderError> {
        self.delegation.acknowledge(ticket).await?;
        self.deferred.lock().await.remove(&ticket.manifest.agent);
        Ok(())
    }

    async fn cancel_outstanding_deferred(&self) -> Result<(), HaiderError> {
        let tickets = self
            .deferred
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for ticket in tickets {
            self.delegation.cancel_ticket(&ticket).await?;
        }
        Ok(())
    }

    async fn cancel(&self) -> Result<(), HaiderError> {
        self.close_effects(true).await
    }

    async fn activate_approval(
        &self,
        run_id: &RunId,
        checkpoint: &RequestInputCheckpoint,
    ) -> Result<(), HaiderError> {
        self.activate_computer_permission(run_id, checkpoint).await
    }

    async fn resolve_approval(&self, menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
        if menu.origin == COMPUTER_PERMISSION_MENU_ORIGIN {
            if answer.option_key.as_deref() != Some("retry") && answer.option_index != 0 {
                return Err(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    "computer OS-permission menu accepts only Retry",
                    false,
                ));
            }
            return Ok(());
        }
        let mut broker = self.broker.lock().await;
        let broker = broker.as_mut().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "tool dispatcher is already closed",
                false,
            )
        })?;
        let mut policy = self.policy.lock().await;
        if broker.permission_menu(&menu.id).is_some() {
            broker.resolve_permission(answer, &mut policy)
        } else {
            let (class, args_digest) = self
                .durable_permission_bindings
                .get(&menu.id)
                .cloned()
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!(
                            "durable effect binding is missing for permission menu {}",
                            menu.id
                        ),
                        false,
                    )
                })?;
            broker.restore_permission(menu, answer, class, args_digest, &mut policy)
        }
        .map_err(tool_error)
    }

    async fn close(&self) -> Result<(), HaiderError> {
        self.close_effects(false).await
    }
}

pub(crate) struct DurableToolState {
    pub(crate) grants: Vec<SessionGrant>,
    pub(crate) bindings: HashMap<MenuId, (EffectClass, String)>,
    pub(crate) freshness: HashMap<String, FileFreshness>,
    pub(crate) mobile_use_active: bool,
}

/// Set to `0`, `false`, `no`, or `off` to keep every computer effect on the
/// ordinary Ask path even after an explicit user computer-use command.
pub const EXPLICIT_COMPUTER_AUTO_GRANT_ENV: &str = "HAIDER_EXPLICIT_COMPUTER_AUTO_GRANT";

fn explicit_computer_auto_grant_enabled() -> bool {
    let value = std::env::var(EXPLICIT_COMPUTER_AUTO_GRANT_ENV).ok();
    explicit_computer_auto_grant_value(value.as_deref())
}

pub(crate) fn explicit_computer_auto_grant_value(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Conservative user-authored opt-in classifier. Consent is the explicit
/// command-like `computer-use` marker, never an inference from prose that may
/// merely discuss or quote computer control.
pub(crate) fn explicit_computer_use_intent(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    let command = normalized.strip_prefix('/').unwrap_or(&normalized);
    command
        .strip_prefix("computer-use")
        .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
}

/// Conservative user-authored mobile capability activation classifier.
/// Consent is the command-like `mobile-use` marker at the start of a root
/// user message, never an inference from prose that mentions the marker.
pub(crate) fn explicit_mobile_use_intent(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }
    let command = normalized.strip_prefix('/').unwrap_or(&normalized);
    command
        .strip_prefix("mobile-use")
        .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
}

fn add_explicit_computer_session_grants(grants: &mut Vec<SessionGrant>) {
    for class in [EffectClass::ScreenObserve, EffectClass::ScreenControl] {
        let grant = SessionGrant::for_effect(class, "explicit-user-computer-use");
        if !grants.contains(&grant) {
            grants.push(grant);
        }
    }
}

pub(crate) async fn durable_session_tool_state(
    store: &dyn StoreHandle,
    session_id: &SessionId,
) -> Result<DurableToolState, HaiderError> {
    let mut cursor = 0;
    let mut intents = HashMap::<EffectId, EffectIntent>::new();
    let mut opened = HashMap::<MenuId, Menu>::new();
    let mut grants = Vec::new();
    let mut bindings = HashMap::new();
    let mut freshness = HashMap::new();
    let mut explicit_computer_intent = false;
    let mut mobile_use_active = false;
    loop {
        let page = store.read(session_id, cursor, 256).await?;
        if page.is_empty() {
            if explicit_computer_intent && explicit_computer_auto_grant_enabled() {
                add_explicit_computer_session_grants(&mut grants);
            }
            return Ok(DurableToolState {
                grants,
                bindings,
                freshness,
                mobile_use_active,
            });
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::Effect(EffectPhase::Intent(intent)) => {
                    intents.insert(intent.effect.clone(), intent);
                }
                EventPayload::Effect(EffectPhase::Authorized {
                    effect,
                    verdict: AuthorizationVerdict::Ask { menu },
                }) => {
                    let Some(intent) = intents.get(&effect) else {
                        continue;
                    };
                    bindings.insert(menu, (intent.class.clone(), intent.args_digest.clone()));
                }
                EventPayload::Effect(EffectPhase::Outcome {
                    freshness: Some(record),
                    ..
                }) => {
                    freshness.insert(record.path.clone(), record);
                }
                EventPayload::MenuOpened(menu)
                    if matches!(menu.kind, MenuKind::Permission { .. }) =>
                {
                    opened.insert(menu.id.clone(), menu);
                }
                EventPayload::MenuAnswered(answer) => {
                    let Some(menu) = opened.get(&answer.menu) else {
                        continue;
                    };
                    let Some(option) = selected_menu_option(menu, &answer) else {
                        continue;
                    };
                    if option.decision != Some(DecisionKind::AllowAlways) {
                        continue;
                    }
                    let Some((class, args_digest)) = bindings.get(&menu.id) else {
                        continue;
                    };
                    let grant = SessionGrant::for_effect(class.clone(), args_digest.clone());
                    if !grants.contains(&grant) {
                        grants.push(grant);
                    }
                }
                EventPayload::UserMessage { text, .. }
                    if envelope.agent_id.is_none() && explicit_computer_use_intent(&text) =>
                {
                    explicit_computer_intent = true;
                }
                EventPayload::UserMessage { text, .. }
                    if envelope.agent_id.is_none() && explicit_mobile_use_intent(&text) =>
                {
                    mobile_use_active = true;
                }
                _ => {}
            }
        }
    }
}

pub(crate) async fn tool_inventory_snapshot(
    store: &dyn StoreHandle,
    session_id: &SessionId,
) -> Result<ToolInventorySnapshot, HaiderError> {
    let durable = durable_session_tool_state(store, session_id).await?;
    let mobile_use_active = durable.mobile_use_active;
    // M2e: `workflow_author` is a GATED child capability — it is surfaced only
    // to a workflow-enabled child through the turn-tools grant path (see the
    // `retain(name != "workflow_author")` on the grantless branch above), never
    // in the general session inventory. Mirror that gate here so a root
    // session's inventory does not advertise it.
    let tools = registered_tools()
        .into_iter()
        .filter(|entry| {
            entry.manifest.name != "workflow_author"
                && (mobile_use_active || entry.manifest.name != "mobile")
        })
        .map(|entry| ToolInventoryEntry {
            manifest: entry.manifest,
            default: entry.default,
        })
        .collect();
    let remembered_grants = durable
        .grants
        .into_iter()
        .filter(|grant| mobile_use_active || grant.class != EffectClass::ReadSms)
        .map(|grant| RememberedSessionGrant {
            class: grant.class,
            scope: match grant.scope {
                SessionGrantScope::Class => RememberedGrantScope::Class,
                SessionGrantScope::CommandShape { args_digest } => {
                    RememberedGrantScope::CommandShape { args_digest }
                }
            },
        })
        .collect();
    Ok(ToolInventorySnapshot {
        tools,
        remembered_grants,
    })
}

fn selected_menu_option<'a>(
    menu: &'a Menu,
    answer: &MenuAnswer,
) -> Option<&'a haider_protocol::menu::MenuOption> {
    match answer.option_key.as_deref() {
        Some(key) => menu.options.iter().find(|option| option.key == key),
        None => usize::try_from(answer.option_index)
            .ok()
            .and_then(|index| menu.options.get(index)),
    }
}

pub(crate) fn typed_tool_result(error: &haider_tools::ToolError) -> Option<BoundedResult> {
    if let haider_tools::ToolError::Computer(error) = error {
        return Some(computer_failure_result(error));
    }
    if let haider_tools::ToolError::Mobile(error) = error {
        return Some(mobile_failure_result(error));
    }
    if let haider_tools::ToolError::StaleRead {
        recorded_digest,
        current_digest,
        ..
    } = error
    {
        return Some(typed_error_result(
            "rejected",
            "stale_read",
            error,
            serde_json::json!({
                "current_digest": current_digest,
                "recorded_digest": recorded_digest,
                "remedy": "re-read before editing",
            }),
        ));
    }
    if let haider_tools::ToolError::EditAnchor(conflict) = error {
        return Some(typed_error_result(
            "conflict",
            "edit_anchor_count",
            error,
            serde_json::json!({
                "matches": conflict.matches,
                "replace_all": conflict.replace_all,
            }),
        ));
    }
    let (status, kind) = match error {
        haider_tools::ToolError::PermissionDenied { .. } => ("denied", "permission_denied"),
        haider_tools::ToolError::WorkspaceBoundary { .. } => ("rejected", "workspace_boundary"),
        haider_tools::ToolError::PathChanged { .. } => ("rejected", "path_changed"),
        haider_tools::ToolError::UnreadFile { .. } => ("rejected", "unread_file"),
        haider_tools::ToolError::EditAnchor(_) => ("conflict", "edit_anchor_count"),
        haider_tools::ToolError::InvalidArgument { .. } => ("rejected", "invalid_argument"),
        haider_tools::ToolError::InvalidMenuAnswer { .. } => ("rejected", "invalid_menu_answer"),
        haider_tools::ToolError::Io { .. } => ("failed", "io"),
        haider_tools::ToolError::Cas { .. } => ("failed", "output_store"),
        haider_tools::ToolError::Runtime { .. } => ("failed", "runtime"),
        haider_tools::ToolError::Computer(_) => unreachable!("handled above"),
        haider_tools::ToolError::Mobile(_) => unreachable!("handled above"),
        haider_tools::ToolError::StaleRead { .. } => ("conflict", "stale_read"),
        haider_tools::ToolError::AuthorizationRequired { .. }
        | haider_tools::ToolError::Journal { .. }
        | haider_tools::ToolError::Ledger { .. }
        | haider_tools::ToolError::Lifecycle { .. } => return None,
    };
    Some(typed_error_result(
        status,
        kind,
        error,
        serde_json::Value::Null,
    ))
}

fn computer_failure_result(error: &ComputerError) -> BoundedResult {
    let (status, reason, presentation) = match error {
        ComputerError::InvalidAction { message } => {
            (ToolResultStatus::Rejected, message.clone(), None)
        }
        ComputerError::PermissionRequired {
            permission,
            settings_pane,
            message,
            ..
        } => (
            ToolResultStatus::Failed,
            message.clone(),
            Some(ErrorPresentation::new(
                format!("computer-{}-required", permission.as_str()),
                format!("Grant {} permission", permission.as_str()),
                format!("{message}. The in-session grant card opens {settings_pane}."),
                ErrorScope::Tool,
                [ErrorAction::Retry],
            )),
        ),
        ComputerError::Cancelled => (
            ToolResultStatus::Cancelled,
            "computer action was cancelled".into(),
            None,
        ),
        ComputerError::Unavailable { message, .. }
        | ComputerError::InspectUnsupported { message, .. }
        | ComputerError::Backend { message } => (ToolResultStatus::Failed, message.clone(), None),
    };
    let error_json = serde_json::to_value(error).unwrap_or_else(|_| {
        serde_json::json!({
            "kind": "backend",
            "message": error.to_string(),
        })
    });
    BoundedResult {
        preview: serde_json::json!({
            "status": match status {
                ToolResultStatus::Rejected => "rejected",
                ToolResultStatus::Cancelled => "cancelled",
                _ => "failed",
            },
            "error": error_json,
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status,
        reason: Some(bounded_failure_reason(&reason)),
        presentation,
    }
}

fn mobile_failure_result(error: &MobileError) -> BoundedResult {
    let (status, reason, subcode, title, actions) = match error {
        MobileError::InvalidAction { message } => (
            ToolResultStatus::Rejected,
            message.clone(),
            "mobile-invalid-action",
            "Mobile action rejected",
            vec![ErrorAction::Retry],
        ),
        MobileError::Cancelled => (
            ToolResultStatus::Cancelled,
            "mobile action was cancelled".into(),
            "mobile-cancelled",
            "Mobile action cancelled",
            vec![ErrorAction::None],
        ),
        MobileError::Unavailable { message } => (
            ToolResultStatus::Failed,
            message.clone(),
            "mobile-backend-unavailable",
            "Mobile backend unavailable",
            vec![ErrorAction::Retry],
        ),
        MobileError::Backend { message } => (
            ToolResultStatus::Failed,
            message.clone(),
            "mobile-backend-failed",
            "Mobile action failed",
            vec![ErrorAction::Retry],
        ),
    };
    let error_json = serde_json::to_value(error).unwrap_or_else(|_| {
        serde_json::json!({
            "kind": "backend",
            "message": error.to_string(),
        })
    });
    BoundedResult {
        preview: serde_json::json!({
            "status": match status {
                ToolResultStatus::Rejected => "rejected",
                ToolResultStatus::Cancelled => "cancelled",
                _ => "failed",
            },
            "error": error_json,
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status,
        reason: Some(bounded_failure_reason(&reason)),
        presentation: Some(ErrorPresentation::new(
            subcode,
            title,
            reason,
            ErrorScope::Tool,
            actions,
        )),
    }
}

fn typed_error_result(
    status: &str,
    kind: &str,
    error: &haider_tools::ToolError,
    details: serde_json::Value,
) -> BoundedResult {
    let mut body = serde_json::json!({
        "status": status,
        "error": {
            "kind": kind,
            "message": error.to_string(),
        }
    });
    if !details.is_null() {
        body["error"]["details"] = details;
    }
    BoundedResult {
        preview: body.to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: match status {
            "denied" | "rejected" => ToolResultStatus::Rejected,
            "conflict" => ToolResultStatus::Conflict,
            _ => ToolResultStatus::Failed,
        },
        reason: Some(bounded_failure_reason(&error.to_string())),
        presentation: None,
    }
}

/// Typed spawn model-selector refusal as a COMPLETED tool result. Static
/// vocabulary plus the caller's own selector strings; the candidates let the
/// model retry with an explicit pair.
fn selection_rejection_result(refusal: &crate::model_select::SelectionRefusal) -> BoundedResult {
    BoundedResult {
        preview: serde_json::json!({
            "status": "rejected",
            "error": {
                "kind": refusal.kind(),
                "message": refusal.message(),
                "details": refusal.details(),
            }
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(bounded_failure_reason(&refusal.message())),
        presentation: None,
    }
}

fn recursion_limit_result() -> BoundedResult {
    BoundedResult {
        preview: serde_json::json!({
            "status": "rejected",
            "error": {
                "kind": "recursion_depth_limit",
                "message": crate::delegation::RECURSION_LIMIT_MESSAGE,
                "limit": crate::delegation::RECURSION_DEPTH_LIMIT,
            }
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(crate::delegation::RECURSION_LIMIT_MESSAGE.into()),
        presentation: None,
    }
}

/// The hard global admission cap is a typed tool rejection, not a parent-turn
/// failure. Preserve the store's E2-E4 presentation verbatim so every client
/// receives the owner-pinned subcode/title/action vocabulary.
fn subagent_limit_result(error: &HaiderError) -> BoundedResult {
    let presentation = error.presentation.clone();
    let details = error.details.clone().unwrap_or(serde_json::Value::Null);
    BoundedResult {
        preview: serde_json::json!({
            "status": "rejected",
            "error": {
                "kind": "subagent_limit_reached",
                "message": error.message,
                "details": details,
            }
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(bounded_failure_reason(&error.message)),
        presentation,
    }
}

fn grant_ceiling_result(name: &str) -> BoundedResult {
    let reason = bounded_failure_reason(&format!(
        "grant ceiling violation: child is not allowed to use `{name}`"
    ));
    BoundedResult {
        preview: serde_json::json!({
            "status": "rejected",
            "error": {
                "kind": "grant_ceiling_violation",
                "message": reason,
                "details": { "tool": name },
            }
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(reason),
        presentation: None,
    }
}

fn mobile_capability_denied_result() -> BoundedResult {
    let reason = bounded_failure_reason(
        "mobile capability is inactive; begin a root user message with `mobile-use` to activate it",
    );
    BoundedResult {
        preview: serde_json::json!({
            "status": "rejected",
            "error": {
                "kind": "capability_denied",
                "message": reason,
                "details": { "tool": "mobile" },
            }
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(reason.clone()),
        presentation: Some(ErrorPresentation::new(
            "capability-denied",
            "Mobile capability inactive",
            reason,
            ErrorScope::Tool,
            [ErrorAction::None],
        )),
    }
}

fn graph_evidence_rejection(
    code: ErrorCode,
    message: &str,
    rejection_kind: Option<&str>,
) -> BoundedResult {
    let reason = bounded_failure_reason(message);
    let subcode = rejection_kind.unwrap_or_else(|| code.as_subcode());
    BoundedResult {
        preview: serde_json::json!({
            "ok": false,
            "code": code.as_str(),
            "kind": rejection_kind,
            "message": reason,
        })
        .to_string(),
        truncated: false,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Rejected,
        reason: Some(reason.clone()),
        presentation: Some(ErrorPresentation::new(
            subcode,
            "Graph evidence rejected",
            reason,
            ErrorScope::Tool,
            [ErrorAction::Retry],
        )),
    }
}

fn request_input_definition() -> ToolDefinition {
    ToolDefinition {
        name: "request_input".into(),
        description: "Ask the user one blocking question or a server-enumerated choice".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["question", "choice"]},
                "title": {"type": "string", "minLength": 1},
                "body": {"type": "array", "items": {"type": "string"}},
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": {"type": "string", "minLength": 1},
                            "label": {"type": "string", "minLength": 1},
                            "detail": {"type": "string"}
                        },
                        "required": ["key", "label"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["kind", "title"],
            "additionalProperties": false
        }),
    }
}

/// E2: plan-gated Loom registration. The gate law: the registration's
/// content must appear inside a plan proposal the human ACCEPTED — the
/// review the plan tool already provides IS the permission.
fn loom_register_definition() -> ToolDefinition {
    ToolDefinition {
        name: "loom_register".into(),
        description: "Register a Loom workflow (kind=workflow, source=pipe text) or agent type                       (kind=agent_type, record object). Requires a plan the human ACCEPTED whose                       body contains the registration content; present one first."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["workflow", "agent_type"]},
                "source": {"type": "string", "minLength": 1, "maxLength": 16384},
                "record": {"type": "object"}
            },
            "required": ["kind"],
            "additionalProperties": false
        }),
    }
}

/// E2 — the pure gate: some accepted plan body must contain EVERY needle.
/// Workflows bind by full pipe source; agent types by id + job + signature
/// (verbatim substrings a proposal card naturally carries).
pub(crate) fn plan_gate_admits(accepted_plan_bodies: &[String], needles: &[&str]) -> bool {
    !needles.is_empty()
        && needles.iter().all(|needle| !needle.trim().is_empty())
        && accepted_plan_bodies
            .iter()
            .any(|body| needles.iter().all(|needle| body.contains(needle.trim())))
}

/// E1 — the registry inventory line for the VOLATILE user tail. Cache law:
/// a registration changes only tail bytes — never durable history, never
/// the system prompt's cache epoch. Names and typed signatures only.
/// The bound agent type's identity line (W-flow): the session IS this
/// specialist for now. Bounded like the inventory; volatile-tail only.
pub(crate) fn agent_type_identity_line(record: &haider_protocol::loom::LoomAgentType) -> String {
    let mut job = record.job.chars().take(700).collect::<String>();
    if job.len() < record.job.len() {
        job.push('…');
    }
    format!(
        "session agent type: @{} ({} -> {}) — {}",
        record.id, record.in_type, record.out_type, job
    )
}

pub(crate) fn loom_inventory_line(
    types: &[haider_protocol::loom::LoomAgentType],
    workflows: &[haider_protocol::loom::LoomWorkflow],
) -> Option<String> {
    if types.is_empty() && workflows.is_empty() {
        return None;
    }
    let mut line = String::from("loom registry —");
    if !types.is_empty() {
        line.push_str(" types:");
        for record in types {
            line.push_str(&format!(
                " @{} {} -> {}",
                record.id, record.in_type, record.out_type
            ));
            line.push(',');
        }
        line.pop();
        line.push(';');
    }
    if !workflows.is_empty() {
        line.push_str(" workflows (run via spawn_subagent(workflow=<id>)):");
        for workflow in workflows {
            line.push_str(&format!(
                " @{} {} -> {}",
                workflow.id, workflow.in_type, workflow.out_type
            ));
            line.push(',');
        }
        line.pop();
        line.push(';');
    }
    line.push_str(" new ones: present the registration in a `plan`, then loom_register.");
    const LOOM_INVENTORY_MAX_BYTES: usize = 700;
    if line.len() > LOOM_INVENTORY_MAX_BYTES {
        let mut end = LOOM_INVENTORY_MAX_BYTES - '…'.len_utf8();
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push('…');
    }
    Some(line)
}

/// D4: the generic plan tool. The full markdown proposal parks the run on a
/// durable menu the client renders full screen; the decision comes back as
/// the tool result.
fn plan_definition() -> ToolDefinition {
    ToolDefinition {
        name: "plan".into(),
        description: "Present a full plan/proposal (markdown) for human review before acting; \
                      returns {decision: accept|revise|reject, note}"
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "minLength": 1, "maxLength": haider_tools::PLAN_TITLE_MAX_BYTES},
                "body": {"type": "string", "minLength": 1, "maxLength": haider_tools::PLAN_BODY_MAX_BYTES}
            },
            "required": ["title", "body"]
        }),
    }
}

fn process_exec_definition() -> ToolDefinition {
    #[cfg(unix)]
    let command_description =
        "Exact shell program passed to /bin/zsh -c when available, otherwise /bin/sh -c";
    #[cfg(windows)]
    let command_description =
        "Exact PowerShell program passed to the absolute System32 Windows PowerShell";
    ToolDefinition {
        name: "process_exec".into(),
        description: "Run one non-interactive shell command inside the session workspace. \
                      Set background=true for long-lived work (servers, watchers, long \
                      builds): the call returns immediately with a task_id, the task \
                      outlives the turn, and its completion is reported back as a session \
                      message; read progress with task_output and stop it with task_kill."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 8192,
                    "description": command_description
                },
                "cwd": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Optional workspace-relative working directory"
                },
                "background": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run as a long-lived background task detached from the turn"
                },
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 80,
                    "description": "Optional display label for a background task \
                                    (defaults to the command's first token)"
                }
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
    }
}

fn required_string(args: &serde_json::Value, field: &str) -> Result<String, HaiderError> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("tool argument `{field}` must be a non-empty string"),
                false,
            )
        })
}

fn required_string_allow_empty(
    args: &serde_json::Value,
    field: &str,
) -> Result<String, HaiderError> {
    args.get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("tool argument `{field}` must be a string"),
                false,
            )
        })
}

fn optional_string(args: &serde_json::Value, field: &str) -> Result<Option<String>, HaiderError> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("tool argument `{field}` must be a non-empty string when provided"),
                false,
            )
        })
}

fn optional_bool(args: &serde_json::Value, field: &str) -> Result<Option<bool>, HaiderError> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    value.as_bool().map(Some).ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("tool argument `{field}` must be a boolean when provided"),
            false,
        )
    })
}

fn optional_u64(args: &serde_json::Value, field: &str) -> Result<Option<u64>, HaiderError> {
    let Some(value) = args.get(field) else {
        return Ok(None);
    };
    value.as_u64().map(Some).ok_or_else(|| {
        HaiderError::new(
            ErrorCode::InvalidArgument,
            format!("tool argument `{field}` must be a non-negative integer when provided"),
            false,
        )
    })
}

#[cfg(all(test, unix))]
pub(crate) fn process_result(result: ProcessResult) -> BoundedResult {
    process_result_with_signal(result, None)
}

fn command_cwd(workspace: &str, requested: Option<&str>) -> PathBuf {
    let workspace = Path::new(workspace);
    requested.map_or_else(
        || workspace.to_path_buf(),
        |requested| {
            let requested = Path::new(requested);
            if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                workspace.join(requested)
            }
        },
    )
}

fn process_output_preview(result: &ProcessResult) -> String {
    let mut bytes = Vec::with_capacity(result.output_bytes);
    for chunk in &result.inline_output {
        if chunk.stream != haider_protocol::item::OutputStream::Stdout {
            continue;
        }
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&chunk.chunk_b64) {
            bytes.extend_from_slice(&decoded);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn process_result_with_signal(
    result: ProcessResult,
    signal: Option<&ProcessSignalRecorded>,
) -> BoundedResult {
    let artifact = result.artifact.clone();
    let truncated = artifact.is_some() || result.limit_reached.is_some();
    let reason = process_failure_reason(&result);
    BoundedResult {
        preview: serde_json::json!({
            "status": result.status,
            "effect_id": result.effect,
            "exit_code": result.exit_code,
            "signal": result.signal,
            "output_bytes": result.output_bytes,
            "command_arg_digest": result.command_arg_digest,
            "transcript_digest": result.transcript_digest,
            "workspace_revision": signal.and_then(|signal| signal.workspace_revision.as_ref()),
            "subject_digest": signal.map(|signal| signal.subject_digest.as_str()),
            "process_signal": signal.map(|signal| ProcessSignalRef {
                run_id: signal.run_id.clone(),
                call_id: signal.call_id.clone(),
                effect_id: signal.effect_id.clone(),
            }),
            "inline_output": result.inline_output,
            "artifact": artifact,
            "limit_reached": result.limit_reached,
            "limits": {
                "wall_timeout_ms": result.wall_timeout_ms,
                "max_output_bytes": result.max_output_bytes,
            },
            "escalation_note": result.escalation_note,
        })
        .to_string(),
        truncated,
        artifact,
        images: Vec::new(),
        cursor: None,
        status: match result.status {
            haider_protocol::item::ToolStatus::Completed => ToolResultStatus::Completed,
            haider_protocol::item::ToolStatus::Rejected => ToolResultStatus::Rejected,
            haider_protocol::item::ToolStatus::Conflict => ToolResultStatus::Conflict,
            haider_protocol::item::ToolStatus::Failed => ToolResultStatus::Failed,
            haider_protocol::item::ToolStatus::Cancelled => ToolResultStatus::Cancelled,
            haider_protocol::item::ToolStatus::Unknown
            | haider_protocol::item::ToolStatus::Pending
            | haider_protocol::item::ToolStatus::InProgress => ToolResultStatus::Unknown,
        },
        reason,
        presentation: None,
    }
}

#[allow(clippy::expect_used)]
fn process_failure_reason(result: &ProcessResult) -> Option<String> {
    match result.status {
        haider_protocol::item::ToolStatus::Completed => None,
        haider_protocol::item::ToolStatus::Cancelled => Some("process cancelled".into()),
        _ if result.limit_reached.is_some() => Some(format!(
            "process exceeded {:?}",
            result.limit_reached.expect("checked")
        )),
        _ if result.exit_code.is_some() => Some(format!(
            "process exited with code {}",
            result.exit_code.expect("checked")
        )),
        _ if result.signal.is_some() => Some(format!(
            "process ended by signal {}",
            result.signal.expect("checked")
        )),
        _ => result
            .escalation_note
            .as_deref()
            .map(bounded_failure_reason)
            .or_else(|| Some("process failed".into())),
    }
}

fn bounded_failure_reason(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(240).collect()
}

#[derive(Clone)]
struct HubArtifactStore {
    store: HubStoreHandle,
}

#[async_trait]
impl CasSink for HubArtifactStore {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.store
            .put_artifact(bytes.to_vec())
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<haider_protocol::ids::ArtifactRef> {
        self.store
            .put_artifact_file(path.to_path_buf())
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })
    }

    async fn put_image(
        &mut self,
        bytes: &[u8],
        media_type: &str,
    ) -> ToolResult<haider_protocol::tool::ImageBlockRef> {
        self.store
            .put_image_artifact(bytes.to_vec(), media_type.to_owned())
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })
    }
}

#[derive(Clone)]
struct HubCommandOutputContext {
    store: HubStoreHandle,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
}

impl HubCommandOutputContext {
    fn sink(
        &self,
        run_id: RunId,
        item_id: ItemId,
        call_id: String,
        prompt: PromptRender,
    ) -> HubCommandOutputSink {
        HubCommandOutputSink {
            store: self.store.clone(),
            branch_id: self.branch_id.clone(),
            agent_id: self.agent_id.clone(),
            run_id,
            item_id,
            call_id,
            prompt,
            device_id: self.device_id.clone(),
            event_ids: Arc::clone(&self.event_ids),
        }
    }

    async fn append_image_created(&self, run_id: &RunId, image: ImageCreatedV1) -> ToolResult<()> {
        let mut identity = blake3::Hasher::new();
        // Round 3: the SESSION joins the identity — event ids are globally
        // unique across sessions, and the same (call_id, path) can
        // legitimately recur in another session.
        identity.update(self.store.session_id().as_str().as_bytes());
        identity.update(b"\x1f");
        identity.update(image.call_id.as_bytes());
        identity.update(b"\x1f");
        identity.update(image.path.as_bytes());
        let identity = identity.finalize().to_hex();
        let item_id = ItemId::new(format!("image-created-{identity}"));
        let data = serde_json::to_value(image).map_err(|error| ToolError::Runtime {
            message: format!("cannot serialize image-created payload: {error}"),
        })?;
        // Verify round 2: the event id derives from the SAME (call_id, path)
        // identity — the store's unique event-id constraint makes a retried
        // or replayed emission an idempotent no-op instead of a duplicate row.
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("image-created-{identity}")),
            seq: 0,
            session_id: self.store.session_id().clone(),
            branch_id: self.branch_id.clone(),
            run_id: Some(run_id.clone()),
            agent_id: self.agent_id.clone(),
            device_id: self.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(EventPayload::Item(ItemEvent::Completed {
                item_id,
                item: TurnItem::Extension {
                    kind: IMAGE_CREATED_EXTENSION_KIND.into(),
                    data,
                },
            }))
            .map_err(|error| ToolError::Runtime {
                message: format!("cannot serialize image-created envelope: {error}"),
            })?,
        }];
        match StoreHandle::append(&self.store, &mut envelopes).await {
            Ok(_) => Ok(()),
            // Round 3: this envelope's event id is DERIVED (session, call,
            // path), so the only InvalidArgument this self-built append can
            // produce is the store's unique-event-id refusal — which means
            // the fact is ALREADY durable. A replayed emission is a no-op,
            // never a tool failure.
            Err(error) if error.code == ErrorCode::InvalidArgument => Ok(()),
            Err(error) => Err(ToolError::Runtime {
                message: error.message,
            }),
        }
    }

    async fn append_permission_payload(
        &self,
        run_id: &RunId,
        payload: PermissionEventPayload,
    ) -> ToolResult<()> {
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.event_ids.next(),
            seq: 0,
            session_id: self.store.session_id().clone(),
            branch_id: self.branch_id.clone(),
            run_id: Some(run_id.clone()),
            agent_id: self.agent_id.clone(),
            device_id: self.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: payload
                .to_payload_value()
                .map_err(|error| ToolError::Runtime {
                    message: format!("cannot serialize computer permission envelope: {error}"),
                })?,
        }];
        StoreHandle::append(&self.store, &mut envelopes)
            .await
            .map_err(|error| ToolError::Runtime {
                message: error.message,
            })?;
        Ok(())
    }

    async fn record_process_signal(
        &self,
        run_id: &RunId,
        result: &ProcessResult,
    ) -> Result<ProcessSignalRecorded, HaiderError> {
        let signal = process_signal_from_result(run_id, result);
        let command = ProcessSignalCommand {
            session_id: self.store.session_id().clone(),
            worker_generation: self.store.worker_generation(),
            branch_id: self.branch_id.clone(),
            signal: signal.clone(),
            stamp_workspace_revision: true,
            device_id: self.device_id.clone(),
        };
        match self.store.hub().record_process_signal(command).await {
            Ok(ProcessSignalOutcome::Committed { signal, .. })
            | Ok(ProcessSignalOutcome::IdempotentReplay { signal, .. }) => Ok(signal),
            Err(SessionHubError::Store(error)) => Err(error),
            Err(error) => Err(HaiderError::new(
                ErrorCode::Internal,
                error.to_string(),
                false,
            )),
        }
    }
}

fn process_signal_from_result(run_id: &RunId, result: &ProcessResult) -> ProcessSignalRecorded {
    let subject_digest = process_signal_subject_digest(
        &result.command_arg_digest,
        &result.transcript_digest,
        result.workspace_revision.as_ref(),
    );
    ProcessSignalRecorded {
        run_id: run_id.clone(),
        call_id: result.call_id.clone(),
        effect_id: result.effect.clone(),
        command_arg_digest: result.command_arg_digest.clone(),
        exit_code: result.exit_code,
        transcript_digest: result.transcript_digest.clone(),
        workspace_revision: result.workspace_revision.clone(),
        subject_digest,
        artifact: result.artifact.clone(),
    }
}

struct HubCommandOutputSink {
    store: HubStoreHandle,
    branch_id: Option<BranchId>,
    agent_id: Option<AgentId>,
    run_id: RunId,
    item_id: ItemId,
    call_id: String,
    prompt: PromptRender,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
}

#[async_trait]
impl CommandOutputSink for HubCommandOutputSink {
    async fn emit(&self, call_id: &str, delta: ItemDelta) -> ToolResult<()> {
        if call_id != self.call_id {
            return Err(haider_tools::ToolError::Runtime {
                message: format!(
                    "process output call id `{call_id}` does not match `{}`",
                    self.call_id
                ),
            });
        }
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.event_ids.next(),
            seq: 0,
            session_id: self.store.session_id().clone(),
            branch_id: self.branch_id.clone(),
            run_id: Some(self.run_id.clone()),
            agent_id: self.agent_id.clone(),
            device_id: self.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: self.prompt,
            },
            payload: serde_json::to_value(EventPayload::Item(ItemEvent::Delta {
                item_id: self.item_id.clone(),
                delta,
            }))
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: format!("cannot serialize command output envelope: {error}"),
            })?,
        }];
        StoreHandle::append(&self.store, &mut envelopes)
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })?;
        Ok(())
    }
}

struct HubJournalSink {
    store: HubStoreHandle,
    run_id: RunId,
    branch_id: Option<BranchId>,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
    diagnostics: Option<Arc<EffectDiagnostics>>,
    workspace_root_digest: String,
    active_tool_name: Arc<StdMutex<Option<String>>>,
    intent_digests: HashMap<EffectId, String>,
    pending_breadcrumbs: HashMap<EffectId, EffectBreadcrumb>,
}

impl HubJournalSink {
    fn new(context: &WorkerToolContext, active_tool_name: Arc<StdMutex<Option<String>>>) -> Self {
        Self {
            store: context.store.clone(),
            run_id: context.run_id.clone(),
            branch_id: context.branch_id.clone(),
            device_id: context.device_id.clone(),
            event_ids: Arc::clone(&context.event_ids),
            diagnostics: context.diagnostics.clone(),
            workspace_root_digest: EffectDiagnostics::workspace_digest(&context.metadata.cwd),
            active_tool_name,
            intent_digests: HashMap::new(),
            pending_breadcrumbs: HashMap::new(),
        }
    }

    fn breadcrumb(&self, effect: &EffectId) -> ToolResult<EffectBreadcrumb> {
        let args_digest =
            self.intent_digests
                .get(effect)
                .cloned()
                .ok_or_else(|| ToolError::Runtime {
                    message: format!(
                        "effect diagnostic dispatch {effect} has no preceding intent digest"
                    ),
                })?;
        let tool_name = self
            .active_tool_name
            .lock()
            .map_err(|_| ToolError::Runtime {
                message: "effect diagnostic tool-name lock is poisoned".into(),
            })?
            .clone()
            .unwrap_or_else(|| "unknown".into());
        Ok(EffectBreadcrumb {
            session_id: self.store.session_id().clone(),
            run_id: self.run_id.clone(),
            effect_id: effect.clone(),
            tool_name,
            workspace_root_digest: self.workspace_root_digest.clone(),
            args_digest,
        })
    }
}

#[async_trait]
impl JournalSink for HubJournalSink {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if let EventPayload::Effect(EffectPhase::Intent(intent)) = &payload {
            self.intent_digests
                .insert(intent.effect.clone(), intent.args_digest.clone());
        }
        let dispatched = match &payload {
            EventPayload::Effect(EffectPhase::Dispatched { effect })
                if self.diagnostics.is_some() =>
            {
                Some(self.breadcrumb(effect)?)
            }
            _ => None,
        };
        let completed = match &payload {
            EventPayload::Effect(EffectPhase::Outcome { effect, .. })
                if self.diagnostics.is_some() =>
            {
                self.pending_breadcrumbs.get(effect).cloned()
            }
            _ => None,
        };
        let mut envelopes = [EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: self.event_ids.next(),
            seq: 0,
            session_id: self.store.session_id().clone(),
            branch_id: self.branch_id.clone(),
            run_id: Some(self.run_id.clone()),
            agent_id: None,
            device_id: self.device_id.clone(),
            authority_epoch: 0,
            worker_generation: self.store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).map_err(|error| {
                haider_tools::ToolError::Runtime {
                    message: format!("cannot serialize effect envelope: {error}"),
                }
            })?,
        }];
        haider_core::StoreHandle::append(&self.store, &mut envelopes)
            .await
            .map_err(|error| haider_tools::ToolError::Runtime {
                message: error.message,
            })?;
        if let (Some(diagnostics), Some(breadcrumb)) = (&self.diagnostics, dispatched) {
            diagnostics
                .record_start(breadcrumb.clone())
                .await
                .map_err(|error| ToolError::Runtime {
                    message: format!("cannot persist pre-dispatch diagnostic breadcrumb: {error}"),
                })?;
            self.pending_breadcrumbs
                .insert(breadcrumb.effect_id.clone(), breadcrumb);
        }
        if let (Some(diagnostics), Some(breadcrumb)) = (&self.diagnostics, completed) {
            diagnostics
                .record_completion(breadcrumb.clone())
                .await
                .map_err(|error| ToolError::Runtime {
                    message: format!("cannot persist completion diagnostic breadcrumb: {error}"),
                })?;
            self.pending_breadcrumbs.remove(&breadcrumb.effect_id);
            self.intent_digests.remove(&breadcrumb.effect_id);
        }
        Ok(())
    }
}

fn tool_error(error: haider_tools::ToolError) -> HaiderError {
    HaiderError::new(ErrorCode::ProviderError, error.to_string(), false)
}

fn computer_error(error: ComputerError) -> HaiderError {
    HaiderError::new(ErrorCode::ProviderError, error.to_string(), false)
}
