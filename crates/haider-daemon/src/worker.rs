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
#[path = "g1_todo_runtime_tests.rs"]
mod g1_todo_runtime_tests;
#[cfg(test)]
#[path = "pair_switch_runtime_tests.rs"]
mod pair_switch_runtime_tests;

use crate::delegation::{DelegationHandle, MessageCoordinates, SpawnCoordinates};
use crate::project_instructions::{self, LoadedProjectInstructions};
use crate::session_hub::{HubStoreHandle, SessionHub, SessionHubError};
use crate::turn_recovery::{cancelled_resumption_payloads, failed_resumption_payloads};
use async_trait::async_trait;
use base64::Engine;
use haider_core::{
    AcceptedShellExec, AcceptedTurn, CancelToken, ChildWaitCheckpoint, ContextCompactionClaim,
    ContextCompactionReceiptResponse, ContextCompactor, DeferredTicket, DeferredToolResult,
    EventIdGenerator, HarnessActor, HarnessConfig, PromptHistoryCompiler, RequestInputCheckpoint,
    StoreHandle, SubmitCheckpointTurn, SubmitChildWaitTurn, SubmitCommittedTurn,
    ToolDispatchResult, ToolDispatcher, TurnHandle, context_soft_threshold_tokens,
    estimate_provider_request_input_tokens, sanitized_failure_message,
};
use haider_protocol::EventPayload;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase, FileFreshness,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind, TreeNode,
};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EffectId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::menu::{DecisionKind, Menu, MenuAnswer, MenuKind};
use haider_protocol::project_instructions::ProjectInstructionsLoaded;
use haider_protocol::provider::{FeatureResolve, FinishReason, StreamEvent};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::{
    BoundedResult, DispatchMode, RememberedGrantScope, RememberedSessionGrant, ToolInventoryEntry,
    ToolInventorySnapshot, ToolManifest, ToolPermissionDefault,
};
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, ANTHROPIC_PROVIDER_NAME, Message, OPENAI_OAUTH_PROVIDER_NAME,
    ResolvedAttachment,
};
use haider_provider::{Provider, ToolDefinition, TurnRequest};
use haider_tools::{
    CasSink, ChangeLedger, CommandOutputSink, EffectBroker, FsCaseMode, FsEdit, FsGlob, FsList,
    FsPatch, FsRead, FsSearch, FsSearchMode, FsWrite, JournalSink, MessageSubagent,
    PermissionPolicy, ProcessBounds, ProcessExec, ProcessResult, ResultBounds, SessionGrant,
    SessionGrantScope, ShellSession, SpawnSubagent, ToolError, ToolResult, TurnAttribution,
    WebFetch,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

const MANAGER_CAPACITY: usize = 128;
const SUPERVISOR_CAPACITY: usize = 64;

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

struct DaemonContextCompactor {
    store: HubStoreHandle,
    provider: Arc<dyn Provider>,
    model: String,
    max_tokens: u64,
    context_window: Option<u64>,
    reserved_output_tokens: u64,
    post_compaction_system_prompt: Option<String>,
    post_compaction_tools: Vec<ToolDefinition>,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
    agent_id: Option<AgentId>,
    branch_id: Option<BranchId>,
}

impl std::fmt::Debug for DaemonContextCompactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonContextCompactor")
            .field("session_id", self.store.session_id())
            .field("model", &self.model)
            .finish_non_exhaustive()
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
        mut covered_messages: Vec<Message>,
    ) -> Result<Message, HaiderError> {
        for message in &mut covered_messages {
            message.blocks.retain(|block| {
                !matches!(
                    block,
                    haider_protocol::provider::Block::Attachment(
                        haider_protocol::tool::AttachmentBlock::Image { .. }
                    )
                )
            });
        }
        covered_messages.push(Message::user_text(
            "Summarize the preceding conversation for lossless continuation. Preserve decisions, constraints, exact identifiers, unresolved work, and tool outcomes. Return only the summary.",
        ));
        let request = TurnRequest {
            messages: covered_messages.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens.min(4096),
            system_prompt: None,
            tools: Vec::new(),
            attachments: Vec::new(),
        };
        let mut stream = self.provider.stream_turn(request).await.map_err(|error| {
            HaiderError::new(
                ErrorCode::ProviderError,
                format!("context summarization could not start: {error}"),
                error.retryable,
            )
        })?;
        let mut summary = String::new();
        let mut finished = false;
        while let Some(item) = stream.recv().await {
            match item.map_err(|error| {
                HaiderError::new(
                    ErrorCode::ProviderError,
                    format!("context summarization failed: {error}"),
                    error.retryable,
                )
            })? {
                StreamEvent::TextDelta { text } => summary.push_str(&text),
                StreamEvent::ReasoningDelta { .. } | StreamEvent::UsageUpdate(_) => {}
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
        let tokens_before = approximate_message_tokens(&covered_messages);
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
    /// W-B: the client web_search executor for this turn (None = typed
    /// unavailable result).
    pub(crate) web_search: Option<Arc<dyn WebSearchExecutor>>,
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
    pub const VERSION: &'static str = "haider-system-v2";

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
    Recover {
        pending: Box<PendingTurn>,
    },
    Nudge {
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
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
    Nudge {
        run_id: RunId,
        accepted_seq: u64,
        text: String,
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
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
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
    checkpoint: Option<RequestInputCheckpoint>,
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
            checkpoint: None,
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

    pub(crate) async fn nudge(
        &self,
        session_id: SessionId,
        run_id: RunId,
        accepted_seq: u64,
        text: String,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .try_send(ManagerCommand::Nudge {
                session_id,
                run_id,
                accepted_seq,
                text,
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
        command: String,
        cwd: Option<String>,
    ) -> Result<(), HaiderError> {
        let (completed, response) = oneshot::channel();
        self.commands
            .try_send(ManagerCommand::ShellExec {
                pending: Box::new(PendingShellExec {
                    accepted,
                    command_id,
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
            checkpoint: Some(checkpoint),
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
            checkpoint: None,
            child_wait: None,
            committed_answer: None,
            recovery_ready: Some(completed),
            recovering: true,
        })?;
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
            checkpoint: None,
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
        let mut terminalized = false;
        for (run_id, state, _, branch_id) in durable_runs(&lease).await? {
            if state.is_terminal() {
                continue;
            }
            if matches!(
                state,
                RunState::InputRequired { .. }
                    | RunState::PermissionRequired { .. }
                    | RunState::Waiting {
                        reason: haider_protocol::state::WaitReason::LocalChild
                    }
            ) {
                // P3-4 (park, don't cancel): a `request_input` checkpoint is
                // durable, resumable state — the P2-6 sweep exists for
                // accepted-WITHOUT-HANDOFF (Queued) runs, and must preserve
                // a parked checkpoint exactly as a crash would; the next
                // generation's recovery reconstructs it (scenario 10).
                continue;
            }
            if state != RunState::Cancelling {
                append_run_state(
                    &lease,
                    &device_id,
                    &run_id,
                    branch_id.as_ref(),
                    &event_ids,
                    RunState::Cancelling,
                )
                .await?;
            }
            reconcile_unknown_effects(&lease, &device_id, &run_id, branch_id.as_ref(), &event_ids)
                .await?;
            let mut payloads = cancelled_resumption_payloads(&lease, &session_id, &run_id).await?;
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
        .filter(|(_, state, _, _)| !state.is_terminal())
        .collect::<Vec<_>>();
    for (run_id, state, _, branch_id) in &runs {
        // Panic can strand a dispatched effect regardless of the run state.
        // Reconcile before either cancellation-shaped or failure-shaped
        // terminalization; a reconciliation error fences every terminal.
        reconcile_unknown_effects(&lease, &device_id, run_id, branch_id.as_ref(), &event_ids)
            .await?;
        if *state == RunState::Cancelling {
            let mut payloads = cancelled_resumption_payloads(&lease, session_id, run_id).await?;
            payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
            append_payloads(
                &lease,
                &device_id,
                run_id,
                branch_id.as_ref(),
                &event_ids,
                payloads,
            )
            .await?;
            continue;
        }
        let error = HaiderError::new(
            ErrorCode::Internal,
            "session supervisor exited before the run completed",
            true,
        );
        let mut payloads = failed_resumption_payloads(&lease, session_id, run_id, &error).await?;
        payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
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
    if !runs.is_empty() {
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
        .acquire_worker_lease_with_cancellation_wake(session_id.clone(), cancellation_wake)
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

trait FutureTurn:
    std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}
impl<T> FutureTurn for T where
    T: std::future::Future<Output = Result<haider_core::TurnOutcome, HaiderError>> + Send
{
}

/// One session's turn loop: strictly serial turns from the bounded queue,
/// with three live inputs while a turn runs — submissions (queued behind the
/// active run), the hub's cancellation wake (durable `Cancelling` reconciled
/// from the journal, active token cancelled), and the active turn's outcome
/// (dispatcher closed, harness stopped and joined, then the store-side
/// conditional Idle settle — `Store::settle_session_idle` owns that law).
/// Shutdown cancels the active turn, terminalizes durable queued runs, and
/// exits only after the last turn settles; the supervisor deregisters its
/// lease on the way out.
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
                    .any(|(_, state, _, _)| state == RunState::RunningTool)
            });
            while !direct_shell_owns_session && let Some(pending) = queue.pop_front() {
                let mut pending = pending;
                let run_id = pending.accepted.run_id.clone();
                let branch_id = pending.accepted.branch_id.clone();
                let recovery_ready = pending.recovery_ready.take();
                let recovering = pending.recovering;
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
                        Some(SupervisorCommand::Nudge {
                            run_id,
                            accepted_seq,
                            text,
                            completed,
                        }) => {
                            let result = if run_id == active_run {
                                if delivered_nudges.insert(accepted_seq) {
                                    turn.harness.nudge(text)
                                } else {
                                    Ok(())
                                }
                            } else {
                                Err(HaiderError::new(
                                    ErrorCode::RunNotActive,
                                    "daemon steer targeted a different active run",
                                    false,
                                ))
                            };
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
                        let (outcome_state, drive_error) = match outcome {
                            Ok(outcome) => (Some(outcome.state), None),
                            Err(error) => (None, Some(error)),
                        };
                        if let Some(dispatcher) = finished.dispatcher.take()
                            && let Err(error) = dispatcher.close().await
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
                            if let Err(reconcile_error) = reconcile_unknown_effects(
                                &lease,
                                &device_id,
                                &finished.run_id,
                                finished.branch_id.as_ref(),
                                &event_ids,
                            )
                            .await
                            {
                                tracing::error!(
                                    run_id = %finished.run_id,
                                    ?reconcile_error,
                                    "failed turn effect reconciliation blocked terminal commit"
                                );
                                let _ = lease.unregister_worker().await;
                                return false;
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
                        // non-terminal in daemon mode. Broker close above first
                        // reconciles every held dispatch to Unknown; only then
                        // may Cancelled become the durable final envelope.
                        if let Err(error) = reconcile_unknown_effects(
                            &lease,
                            &device_id,
                            &finished.run_id,
                            finished.branch_id.as_ref(),
                            &event_ids,
                        )
                        .await
                        {
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
                        let durable = durable_run_state(&lease, &finished.run_id).await;
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
    true
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
            .find_map(|(candidate, state, _, _)| (candidate == run_id).then_some(state))
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
    for (run_id, state, accepted_seq, branch_id) in runs {
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
        queue.push_back(PendingTurn::accepted(AcceptedTurn {
            session_id: store.session_id().clone(),
            run_id,
            accepted_seq,
            worker_generation: store.worker_generation(),
            branch_id,
            disposition: haider_core::TurnAdmissionDisposition::Queued,
            first_user_turn: false,
        }));
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
    for (run_id, state, _, branch_id) in runs {
        if state != RunState::Cancelling {
            continue;
        }
        if active_run == Some(&run_id) {
            if let Some((_, cancel)) = active {
                cancel.cancel();
            }
            continue;
        }
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
) -> Result<Vec<(RunId, RunState, Option<u64>, Option<BranchId>)>, HaiderError> {
    let mut cursor = 0;
    let mut runs = HashMap::<RunId, (RunState, Option<u64>, Option<BranchId>)>::new();
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
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::RunState(state) => {
                    let (accepted, branch_id) = runs
                        .get(&run_id)
                        .map_or((None, envelope.branch_id.clone()), |(_, seq, branch_id)| {
                            (*seq, branch_id.clone())
                        });
                    if branch_id != envelope.branch_id {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} crosses branch scopes"),
                            false,
                        ));
                    }
                    runs.insert(run_id, (state, accepted, branch_id));
                }
                EventPayload::UserMessage { .. } => {
                    let (state, branch_id) = runs.get(&run_id).map_or(
                        (RunState::Queued, envelope.branch_id.clone()),
                        |(state, _, branch_id)| (state.clone(), branch_id.clone()),
                    );
                    if branch_id != envelope.branch_id {
                        return Err(HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("run {run_id} crosses branch scopes"),
                            false,
                        ));
                    }
                    runs.insert(run_id, (state, Some(envelope.seq), branch_id));
                }
                _ => {}
            }
        }
    }
    let mut runs = runs
        .into_iter()
        .map(|(run_id, (state, accepted, branch_id))| (run_id, state, accepted, branch_id))
        .collect::<Vec<_>>();
    runs.sort_by_key(|(_, _, accepted, _)| accepted.unwrap_or(u64::MAX));
    Ok(runs)
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
) -> Result<(), HaiderError> {
    let mut dispatched = HashSet::<EffectId>::new();
    let mut terminal = HashSet::<EffectId>::new();
    let mut cursor = 0;
    loop {
        let page = StoreHandle::read(store, store.session_id(), cursor, 512).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id)
                || envelope.branch_id.as_ref() != branch_id
                || envelope
                    .payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    != Some("effect")
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
                EventPayload::Effect(EffectPhase::Dispatched { effect }) => {
                    dispatched.insert(effect);
                }
                EventPayload::Effect(EffectPhase::Outcome { effect, .. }) => {
                    terminal.insert(effect);
                }
                _ => {}
            }
        }
    }
    let mut pending = dispatched
        .difference(&terminal)
        .cloned()
        .collect::<Vec<_>>();
    pending.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    if pending.is_empty() {
        return Ok(());
    }
    append_payloads(
        store,
        device_id,
        run_id,
        branch_id,
        event_ids,
        pending
            .into_iter()
            .map(|effect| {
                EventPayload::Effect(EffectPhase::Outcome {
                    effect,
                    outcome: EffectOutcome::Unknown,
                    freshness: None,
                })
            })
            .collect(),
    )
    .await
}

async fn durable_run_state(store: &HubStoreHandle, run_id: &RunId) -> Option<RunState> {
    durable_runs(store).await.ok().and_then(|runs| {
        runs.into_iter()
            .find_map(|(candidate, state, _, _)| (candidate == *run_id).then_some(state))
    })
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
    for (run_id, state, _, branch_id) in durable_runs(store).await.unwrap_or_default() {
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
            .any(|(_, state, _, _)| !state.is_terminal())
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
    let agent_id = delegation.agent_for_session(lease.session_id()).await?;
    let web_degrade = lease.hub().web_degrade(lease.session_id());
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
    let instructions = project_instructions::load(&metadata.cwd).await;
    let instruction_entries = instructions
        .as_ref()
        .map_or_else(Vec::new, LoadedProjectInstructions::prompt_entries);
    let handoff_dir = delegation
        .handoff_dir_for_child_session(lease.session_id(), &metadata.cwd)
        .await?;
    let post_compaction_system_prompt = SystemPromptBuilder::build_with_handoff(
        metadata,
        &instruction_entries,
        handoff_dir.as_deref(),
    );
    let post_compaction_tools = advertised_tool_definitions(
        &dependencies.tool_factory,
        agent_id.is_some(),
        &resolved.provider_name,
        web_degrade,
    );
    let mut messages = PromptHistoryCompiler::compile_idle_with_artifacts(
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
    let compactor = DaemonContextCompactor {
        store: lease.clone(),
        provider: resolved.provider,
        model: resolved.model,
        max_tokens: metadata.max_tokens,
        context_window: resolved.context_window,
        reserved_output_tokens: metadata.max_tokens,
        post_compaction_system_prompt: Some(post_compaction_system_prompt),
        post_compaction_tools,
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
        agent_id,
        branch_id: branch_id.clone(),
    };
    if let Err(error) = compactor.compact(&run_id, &intent, messages).await {
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

async fn perform_shell_exec(
    metadata: &SessionMetadataV1,
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: Arc<EventIdGenerator>,
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
        return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
    }
    if state != Some(RunState::RunningTool) {
        return Err(HaiderError::new(
            ErrorCode::RunNotActive,
            format!("direct shell run {run_id} is not durably running"),
            false,
        ));
    }
    if *drain_wakes.borrow() {
        begin_shell_cancellation(lease, device_id, &event_ids, &run_id).await?;
        return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
    }

    let shell = match ShellSession::new(&metadata.cwd, Vec::new()) {
        Ok(shell) => shell,
        Err(error) => {
            return fail_shell_exec(lease, device_id, &event_ids, &run_id, tool_error(error)).await;
        }
    };
    let operation = match shell.prepare_user_process(
        pending.command_id.clone(),
        pending.command.clone(),
        pending.cwd.as_deref().map(Path::new),
    ) {
        Ok(operation) => operation,
        Err(error) => {
            return fail_shell_exec(lease, device_id, &event_ids, &run_id, tool_error(error)).await;
        }
    };
    let journal = HubJournalSink {
        store: lease.clone(),
        run_id: run_id.clone(),
        branch_id: None,
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
    };
    let mut broker = match EffectBroker::new(
        Box::new(journal),
        &metadata.cwd,
        lease.session_id().clone(),
        lease.worker_generation(),
    ) {
        Ok(broker) => broker,
        Err(error) => {
            return fail_shell_exec(lease, device_id, &event_ids, &run_id, tool_error(error)).await;
        }
    };
    let output = HubCommandOutputContext {
        store: lease.clone(),
        branch_id: None,
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
    }
    .sink(
        run_id.clone(),
        pending.accepted.item_id.clone(),
        pending.command_id,
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
            return fail_shell_exec(lease, device_id, &event_ids, &run_id, error).await;
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
                    begin_shell_cancellation(lease, device_id, &event_ids, &run_id).await?;
                    process_cancel.cancel();
                    let _ = wait.await;
                    if let Err(error) = broker.close().await {
                        tracing::warn!(
                            %run_id,
                            ?error,
                            "direct shell broker close reported an error during daemon drain"
                        );
                    }
                    return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
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
                    if let Err(error) = broker.close().await {
                        tracing::warn!(
                            %run_id,
                            ?error,
                            "direct shell broker close reported an error during cancellation"
                        );
                    }
                    return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
                }
            }
            result = &mut wait => {
                break match result {
                    Ok(result) => result,
                    Err(error) => {
                        let error = tool_error(error);
                        let _ = broker.close().await;
                        return fail_shell_exec(lease, device_id, &event_ids, &run_id, error).await;
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
            HaiderError::new(
                ErrorCode::EffectUnknownOutcome,
                format!("direct shell broker close reported unfinished work: {error}"),
                false,
            ),
        )
        .await;
    }
    if durable_run_state(lease, &run_id).await == Some(RunState::Cancelling) {
        return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
    }
    let completed = append_payloads(
        lease,
        device_id,
        &run_id,
        None,
        &event_ids,
        vec![
            EventPayload::Item(ItemEvent::Completed {
                item_id: pending.accepted.item_id,
                item: result.completed_item(pending.command),
            }),
            EventPayload::RunState(RunState::Done),
        ],
    )
    .await;
    if let Err(error) = completed {
        if durable_run_state(lease, &run_id).await == Some(RunState::Cancelling) {
            return cancel_shell_exec(lease, device_id, &event_ids, &run_id).await;
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
) -> Result<(), HaiderError> {
    match durable_run_state(lease, run_id).await {
        Some(RunState::RunningTool) => {
            append_run_state(
                lease,
                device_id,
                run_id,
                None,
                event_ids,
                RunState::Cancelling,
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
) -> Result<(), HaiderError> {
    reconcile_unknown_effects(lease, device_id, run_id, None, event_ids).await?;
    let mut payloads = cancelled_resumption_payloads(lease, lease.session_id(), run_id).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    append_payloads(lease, device_id, run_id, None, event_ids, payloads).await?;
    append_session_idle(lease, device_id, event_ids, true).await
}

async fn fail_shell_exec(
    lease: &HubStoreHandle,
    device_id: &DeviceId,
    event_ids: &EventIdGenerator,
    run_id: &RunId,
    error: HaiderError,
) -> Result<(), HaiderError> {
    if durable_run_state(lease, run_id).await == Some(RunState::Cancelling) {
        return cancel_shell_exec(lease, device_id, event_ids, run_id).await;
    }
    reconcile_unknown_effects(lease, device_id, run_id, None, event_ids).await?;
    let mut payloads =
        failed_resumption_payloads(lease, lease.session_id(), run_id, &error).await?;
    payloads.retain(|payload| !matches!(payload, EventPayload::SessionState(_)));
    if let Err(append_error) =
        append_payloads(lease, device_id, run_id, None, event_ids, payloads).await
    {
        if durable_run_state(lease, run_id).await == Some(RunState::Cancelling) {
            return cancel_shell_exec(lease, device_id, event_ids, run_id).await;
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
        checkpoint,
        child_wait,
        mut committed_answer,
        recovery_ready: _,
        recovering: _,
    } = pending;
    // W-B (decision 8): the session's web-capability degrades ride into
    // pair resolution (native declarations) AND the tool pack below — ONE
    // per-turn derivation from the resolved pair.
    let web_degrade = lease.hub().web_degrade(lease.session_id());
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
    let delegation = dependencies.delegation.clone().ok_or_else(|| {
        HaiderError::new(
            ErrorCode::Internal,
            "worker delegation coordinator is not installed",
            false,
        )
    })?;
    let agent_id = delegation.agent_for_session(lease.session_id()).await?;
    // G1 (L5): a delegation-owned session is a child — its tool pack below
    // excludes the root-only planning surface.
    let delegated_child = agent_id.is_some();
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
    let prompt_compile_started = Instant::now();
    let mut messages = PromptHistoryCompiler::compile_with_artifacts(
        lease,
        lease,
        lease.session_id(),
        accepted.branch_id.as_ref(),
        agent_id.as_ref(),
        &accepted.run_id,
    )
    .await?;
    // G3 (LT3): provider-opaque continuation facts are only valid on the
    // wire family that minted them — every adapter REJECTS a foreign tag.
    // After a cross-provider model switch the compiled history still carries
    // the old family's facts (openai encrypted reasoning, gemini signed
    // parts, anthropic thinking blocks), so they are stripped here, before
    // the request, instead of failing every turn on the new pair.
    strip_foreign_provider_opaque(&mut messages, &resolved.provider_name);
    if messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { .. }
                )
            )
        })
    }) && resolved.provider.capabilities().await.vision == FeatureResolve::Unsupported
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
    let attachments = resolve_prompt_attachments(lease, &mut messages).await?;
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
            web_search: dependencies.web_search.clone(),
        })
        .await?;
    let mut config = HarnessConfig::for_session(
        lease.session_id().clone(),
        device_id.clone(),
        0,
        lease.worker_generation(),
    )
    .with_event_ids(Arc::clone(&event_ids));
    config.cached_input_is_subset = !matches!(
        resolved.provider_name.as_str(),
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    );
    config.context_compaction_v1 = true;
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
    config.tools = advertised_tool_definitions(
        &dependencies.tool_factory,
        delegated_child,
        &resolved.provider_name,
        web_degrade,
    );
    // W6c children retain the spawn tool. The coordinator derives their
    // durable depth from the parent delegation and returns a typed tool
    // result at the cap; hiding the tool would turn that recoverable model
    // decision into provider-specific behavior. G1: children do NOT retain
    // `todo_write` — the plan surface is root-only (L5).
    config.attachments = attachments;
    config.context_compactor = Some(Arc::new(DaemonContextCompactor {
        store: lease.clone(),
        provider: Arc::clone(&resolved.provider),
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        context_window: config.context_window,
        reserved_output_tokens: config.reserved_output_tokens,
        post_compaction_system_prompt: config.system_prompt.clone(),
        post_compaction_tools: config.tools.clone(),
        device_id: device_id.clone(),
        event_ids: Arc::clone(&event_ids),
        agent_id: config.agent_id.clone(),
        branch_id: accepted.branch_id.clone(),
    }));
    config.usage_account = resolved
        .account_alias
        .map(haider_protocol::ids::CredentialAlias::new);
    config.rotation_budget_consumed = resolved.rotation_budget_consumed;
    config.initial_rotation = resolved.initial_rotation;
    config.provider_attempt_resolver = resolved.attempt_resolver;
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
    let (actor, harness) = HarnessActor::new_with_dispatcher(
        config,
        resolved.provider,
        Arc::new(lease.clone()),
        dispatcher.clone(),
    );
    match checkpoint.as_ref() {
        Some(checkpoint) => {
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
        None => {
            lease
                .register_harness(harness.clone())
                .await
                .map_err(hub_error)?;
        }
    }
    if committed_answer.is_none()
        && let Some(checkpoint) = checkpoint.as_ref()
    {
        committed_answer =
            find_committed_menu_answer(lease, accepted.branch_id.as_ref(), &checkpoint.menu.id)
                .await?;
    }
    if let Some(answer) = committed_answer {
        harness.apply_committed_menu_event(answer)?;
    }
    let actor = AbortOnDropTask::new(tokio::spawn(actor.run()));
    let submitted = match (checkpoint, child_wait) {
        (Some(checkpoint), None) => {
            harness
                .submit_checkpoint_turn(SubmitCheckpointTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        (None, Some(checkpoint)) => {
            harness
                .submit_child_wait_turn(SubmitChildWaitTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                    checkpoint,
                })
                .await
        }
        (None, None) => {
            harness
                .submit_committed_turn(SubmitCommittedTurn {
                    run_id: accepted.run_id.clone(),
                    messages,
                })
                .await
        }
        (Some(_), Some(_)) => Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            "recovered turn contains two checkpoint kinds",
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

async fn resolve_prompt_attachments(
    store: &HubStoreHandle,
    messages: &mut [Message],
) -> Result<Vec<ResolvedAttachment>, HaiderError> {
    let mut resolved = Vec::<ResolvedAttachment>::new();
    for message in messages {
        for block in &mut message.blocks {
            match block {
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
                    let bytes = store.get_artifact(artifact.clone()).await?;
                    resolved.push(ResolvedAttachment {
                        artifact,
                        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    });
                }
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::PastedText { artifact, .. },
                ) => {
                    let artifact = artifact.clone();
                    let bytes = store.get_artifact(artifact.clone()).await?;
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
                    let bytes = store.get_artifact(artifact.clone()).await?;
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
    Ok(resolved)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegisteredToolRoute {
    RequestInput,
    TodoWrite,
    FsRead,
    FsList,
    FsSearch,
    FsGlob,
    FsWrite,
    FsEdit,
    FsPatch,
    ProcessExec,
    SpawnSubagent,
    MessageSubagent,
    TaskOutput,
    TaskKill,
    WebFetch,
    WebSearch,
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
        {
            // G1: actor-owned like request_input — no brokered effect.
            let manifest = haider_tools::todo_write_manifest();
            RegisteredTool {
                manifest,
                default: ToolPermissionDefault::NotApplicable,
                route: RegisteredToolRoute::TodoWrite,
            }
        },
        registered_tool(
            tool_definition("fs_read", "Read a UTF-8 file", &["path"]),
            vec![EffectClass::FsRead],
            DispatchMode::Await,
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsRead,
        ),
        registered_tool(
            tool_definition("fs_list", "List a directory", &["path"]),
            vec![EffectClass::FsRead],
            DispatchMode::Await,
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsList,
        ),
        registered_tool(
            fs_search_definition(),
            vec![EffectClass::FsRead],
            DispatchMode::Await,
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsSearch,
        ),
        registered_tool(
            fs_glob_definition(),
            vec![EffectClass::FsRead],
            DispatchMode::Await,
            ToolPermissionDefault::Allow,
            RegisteredToolRoute::FsGlob,
        ),
        registered_tool(
            fs_write_definition(),
            vec![EffectClass::FsWrite],
            DispatchMode::Await,
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsWrite,
        ),
        registered_tool(
            fs_patch_definition(),
            vec![EffectClass::FsWrite],
            DispatchMode::Await,
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsPatch,
        ),
        registered_tool(
            fs_edit_definition(),
            vec![EffectClass::FsWrite],
            DispatchMode::Await,
            ToolPermissionDefault::Ask,
            RegisteredToolRoute::FsEdit,
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
    ]
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

fn provider_definition(manifest: ToolManifest) -> ToolDefinition {
    ToolDefinition {
        name: manifest.name,
        description: manifest.description,
        input_schema: manifest.input_schema,
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
pub(crate) fn advertised_tool_definitions(
    tool_factory: &Arc<dyn TurnToolFactory>,
    delegated_child: bool,
    provider_name: &str,
    web_degrade: WebCapabilityDegrade,
) -> Vec<ToolDefinition> {
    let mut definitions = tool_factory.definitions();
    if delegated_child {
        definitions.retain(|definition| definition.name != "todo_write");
    }
    // First-party Anthropic pairs carry the SERVER `web_fetch` tool, so the
    // local client tool is withheld — UNLESS the session's server tools
    // degraded (400), which is exactly the "local fallback on refusal".
    if matches!(
        provider_name,
        ANTHROPIC_PROVIDER_NAME | ANTHROPIC_OAUTH_PROVIDER_NAME
    ) && !web_degrade.anthropic_web_tools
    {
        definitions.retain(|definition| definition.name != "web_fetch");
    }
    // The client `web_search` tool exists for responses-lite pairs only —
    // every other family either has a provider-native search or honestly
    // none — and a latched 404/410 stops advertising it for the session.
    if provider_name != OPENAI_OAUTH_PROVIDER_NAME || web_degrade.openai_alpha_search {
        definitions.retain(|definition| definition.name != "web_search");
    }
    definitions
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
        let durable_permissions =
            durable_session_tool_state(&context.store, context.store.session_id()).await?;
        let session_id = context.store.session_id().clone();
        let journal = HubJournalSink::new(&context);
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
                ToolPermissionDefault::Deny => {
                    policy.deny(class, "denied by daemon default policy")
                }
                ToolPermissionDefault::NotApplicable => {}
            }
        }
        for grant in durable_permissions.grants {
            policy.allow_session_grant(grant).map_err(tool_error)?;
        }
        let output = HubCommandOutputContext {
            store: context.store.clone(),
            branch_id: context.branch_id.clone(),
            device_id: context.device_id.clone(),
            event_ids: Arc::clone(&context.event_ids),
        };
        Ok(Some(Arc::new(BrokerToolDispatcher {
            broker: Mutex::new(Some(broker)),
            web_search: context.web_search.clone(),
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
            deferred: Mutex::new(HashMap::new()),
        })))
    }
}

pub(crate) fn effective_permission_defaults(
    metadata: &SessionMetadataV1,
) -> Vec<(EffectClass, ToolPermissionDefault)> {
    let overrides = metadata.permission_overrides.unwrap_or_default();
    registered_tools()
        .into_iter()
        .flat_map(|entry| {
            entry.manifest.effects.into_iter().map(move |class| {
                let default = if (overrides.allow_writes && class == EffectClass::FsWrite)
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
                (class, default)
            })
        })
        .collect()
}

struct BrokerToolDispatcher {
    broker: Mutex<Option<EffectBroker>>,
    web_search: Option<Arc<dyn WebSearchExecutor>>,
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
    deferred: Mutex<HashMap<AgentId, DeferredTicket>>,
}

#[async_trait]
impl ToolDispatcher for BrokerToolDispatcher {
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
        let mut broker_guard = self.broker.lock().await;
        let broker = broker_guard.as_mut().ok_or_else(|| {
            HaiderError::new(
                ErrorCode::Internal,
                "tool dispatcher is already closed",
                false,
            )
        })?;
        let policy = self.policy.lock().await;
        let route = registered_tool_route(name).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::InvalidArgument,
                format!("unsupported tool `{name}`"),
                false,
            )
        })?;
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
            let request = SpawnSubagent::from_tool_args(args).map_err(tool_error)?;
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
                        cursor: None,
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
                cursor: None,
            }));
        }
        let result = match route {
            RegisteredToolRoute::FsRead => {
                let path = required_string(&args, "path")?;
                let mut cas = self.cas.lock().await;
                broker
                    .fs_read(
                        &FsRead::new(path),
                        &policy,
                        &mut *cas,
                        ResultBounds::default(),
                    )
                    .await
            }
            RegisteredToolRoute::FsList => {
                let path = required_string(&args, "path")?;
                let mut cas = self.cas.lock().await;
                broker
                    .fs_list(
                        &FsList::new(path),
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
                let cwd = optional_string(&args, "cwd")?;
                let background = optional_bool(&args, "background")?.unwrap_or(false);
                if background {
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
                    let mut operation = ProcessExec::new(call_id, command);
                    if let Some(cwd) = cwd {
                        operation = operation.with_cwd(cwd);
                    }
                    let cas = self.cas.lock().await.clone();
                    let output =
                        self.output
                            .sink(run_id.clone(), item_id.clone(), call_id.to_owned());
                    match broker
                        .process_exec(&operation, &policy, cas, output, ProcessBounds::default())
                        .await
                    {
                        Ok(execution) => execution.wait().await.map(process_result),
                        Err(error) => Err(error),
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
                match broker.begin_web_fetch(&operation, &policy).await {
                    Ok(intent) => {
                        let fetched = tokio::select! {
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
                            fetched = haider_provider::fetch_public_url(
                                operation.url(),
                                operation.max_bytes(),
                            ) => fetched,
                        };
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
                                    cursor: None,
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
                                    cursor: None,
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
                broker
                    .fs_write(
                        &FsWrite::new(path, content),
                        &policy,
                        &attribution,
                        &self.ledger,
                    )
                    .await
            }
            RegisteredToolRoute::FsPatch => {
                let path = required_string(&args, "path")?;
                let patch = args.get("patch").ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::InvalidArgument,
                        "tool argument `patch` must be an object",
                        false,
                    )
                })?;
                let preimage = required_string(patch, "preimage")?;
                let replacement = required_string_allow_empty(patch, "replacement")?;
                let attribution = TurnAttribution::new(self.session_id.clone(), run_id.clone());
                broker
                    .fs_patch(
                        &FsPatch::new(path, preimage, replacement),
                        &policy,
                        &attribution,
                        &self.ledger,
                    )
                    .await
            }
            RegisteredToolRoute::FsEdit => {
                let path = required_string(&args, "path")?;
                let old_string = required_string(&args, "old_string")?;
                let new_string = required_string_allow_empty(&args, "new_string")?;
                let replace_all = optional_bool(&args, "replace_all")?.unwrap_or(false);
                let attribution = TurnAttribution::new(self.session_id.clone(), run_id.clone());
                broker
                    .fs_edit(
                        &FsEdit::new(path, old_string, new_string).replace_all(replace_all),
                        &policy,
                        &attribution,
                        &self.ledger,
                    )
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
                        cursor: None,
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
                                    cursor: None,
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
                                    cursor: None,
                                })
                            }
                        }
                    }
                }
            }
            RegisteredToolRoute::RequestInput
            | RegisteredToolRoute::TodoWrite
            | RegisteredToolRoute::SpawnSubagent
            | RegisteredToolRoute::MessageSubagent => {
                return Err(HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("tool `{name}` is not dispatched by the general-tool match"),
                    false,
                ));
            }
        };
        match result {
            Ok(result) => Ok(ToolDispatchResult::Completed(result)),
            Err(haider_tools::ToolError::AuthorizationRequired { menu }) => {
                let menu = broker.permission_menu(&menu).cloned().ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        "broker authorization menu disappeared before publication",
                        false,
                    )
                })?;
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

    async fn resolve_approval(&self, menu: &Menu, answer: &MenuAnswer) -> Result<(), HaiderError> {
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
        let broker = self.broker.lock().await.take();
        let Some(broker) = broker else {
            return Ok(());
        };
        broker.close().await.map(|_| ()).map_err(|error| {
            HaiderError::new(
                ErrorCode::EffectUnknownOutcome,
                format!("effect broker close reported unfinished work: {error}"),
                false,
            )
        })
    }
}

pub(crate) struct DurableToolState {
    pub(crate) grants: Vec<SessionGrant>,
    pub(crate) bindings: HashMap<MenuId, (EffectClass, String)>,
    pub(crate) freshness: HashMap<String, FileFreshness>,
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
    loop {
        let page = store.read(session_id, cursor, 256).await?;
        if page.is_empty() {
            return Ok(DurableToolState {
                grants,
                bindings,
                freshness,
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
    let tools = registered_tools()
        .into_iter()
        .map(|entry| ToolInventoryEntry {
            manifest: entry.manifest,
            default: entry.default,
        })
        .collect();
    let remembered_grants = durable
        .grants
        .into_iter()
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
        haider_tools::ToolError::Conflict(_) => ("conflict", "patch_conflict"),
        haider_tools::ToolError::InvalidArgument { .. } => ("rejected", "invalid_argument"),
        _ => return None,
    };
    Some(typed_error_result(
        status,
        kind,
        error,
        serde_json::Value::Null,
    ))
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
        cursor: None,
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
        cursor: None,
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
        cursor: None,
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

fn fs_search_definition() -> ToolDefinition {
    ToolDefinition {
        name: "fs_search".into(),
        description: "Search UTF-8 files by literal or simple wildcard pattern".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1},
                "path": {"type": "string", "minLength": 1},
                "glob": {"type": "string", "minLength": 1},
                "case": {
                    "type": "string",
                    "enum": ["sensitive", "insensitive", "smart"]
                },
                "mode": {
                    "type": "string",
                    "enum": ["literal", "simple"],
                    "description": "simple supports dependency-free * and ? wildcards"
                },
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Legacy alias for pattern"
                },
                "root": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Legacy alias for path"
                }
            },
            "anyOf": [
                {"required": ["pattern"]},
                {"required": ["query"]}
            ],
            "additionalProperties": false
        }),
    }
}

fn fs_glob_definition() -> ToolDefinition {
    ToolDefinition {
        name: "fs_glob".into(),
        description: "List workspace files matching a bounded glob".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1},
                "path": {"type": "string", "minLength": 1}
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
    }
}

fn fs_write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "fs_write".into(),
        description: "Create or replace one UTF-8 file after explicit approval".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "content": {"type": "string"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    }
}

fn fs_patch_definition() -> ToolDefinition {
    ToolDefinition {
        name: "fs_patch".into(),
        description:
            "Apply one exact-preimage structured hunk to a UTF-8 file after explicit approval"
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "patch": {
                    "type": "object",
                    "properties": {
                        "preimage": {"type": "string", "minLength": 1},
                        "replacement": {"type": "string"}
                    },
                    "required": ["preimage", "replacement"],
                    "additionalProperties": false
                }
            },
            "required": ["path", "patch"],
            "additionalProperties": false
        }),
    }
}

fn fs_edit_definition() -> ToolDefinition {
    ToolDefinition {
        name: "fs_edit".into(),
        description: "Replace one exact string anchor, or every occurrence, in a fresh UTF-8 file"
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "old_string": {"type": "string", "minLength": 1},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        }),
    }
}

fn tool_definition(name: &str, description: &str, required: &[&str]) -> ToolDefinition {
    let properties = required
        .iter()
        .map(|name| ((*name).to_owned(), serde_json::json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

fn process_exec_definition() -> ToolDefinition {
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
                    "description": "Exact shell program passed to /bin/sh -c"
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

fn process_result(result: ProcessResult) -> BoundedResult {
    let artifact = result.artifact.clone();
    let truncated = artifact.is_some() || result.limit_reached.is_some();
    BoundedResult {
        preview: serde_json::json!({
            "status": result.status,
            "exit_code": result.exit_code,
            "output_bytes": result.output_bytes,
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
        cursor: None,
    }
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
}

#[derive(Clone)]
struct HubCommandOutputContext {
    store: HubStoreHandle,
    branch_id: Option<BranchId>,
    device_id: DeviceId,
    event_ids: Arc<EventIdGenerator>,
}

impl HubCommandOutputContext {
    fn sink(&self, run_id: RunId, item_id: ItemId, call_id: String) -> HubCommandOutputSink {
        HubCommandOutputSink {
            store: self.store.clone(),
            branch_id: self.branch_id.clone(),
            run_id,
            item_id,
            call_id,
            device_id: self.device_id.clone(),
            event_ids: Arc::clone(&self.event_ids),
        }
    }
}

struct HubCommandOutputSink {
    store: HubStoreHandle,
    branch_id: Option<BranchId>,
    run_id: RunId,
    item_id: ItemId,
    call_id: String,
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
}

impl HubJournalSink {
    fn new(context: &WorkerToolContext) -> Self {
        Self {
            store: context.store.clone(),
            run_id: context.run_id.clone(),
            branch_id: context.branch_id.clone(),
            device_id: context.device_id.clone(),
            event_ids: Arc::clone(&context.event_ids),
        }
    }
}

#[async_trait]
impl JournalSink for HubJournalSink {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
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
        Ok(())
    }
}

fn tool_error(error: haider_tools::ToolError) -> HaiderError {
    HaiderError::new(ErrorCode::ProviderError, error.to_string(), false)
}
