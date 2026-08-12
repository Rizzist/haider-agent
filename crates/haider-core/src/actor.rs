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
//!   events. `wait_before_provider_retry` commits durable `Retrying` state
//!   (W-C M4: a visible `attempt K/max` counter, Claude-Code style) around the
//!   backoff and cancellation wins every wait. The backoff is a PURE function
//!   of the attempt (`retry_backoff_ms`), and the wait is served through the
//!   injected [`RetrySleeper`] so laws assert the sequence without waiting.
//!
//! General tool calls run through the injected [`ToolDispatcher`] (W3c);
//! with no dispatcher installed they are surfaced completed-as-
//! `ToolStatus::Pending`, the pre-W3c standalone behavior. `request_input`
//! is the intentional exception: the actor owns its blocking menu round trip
//! because only the actor may journal the session's
//! `MenuOpened`/`MenuAnswered` and run-state envelopes. Event ids come from
//! the [`EventIdGenerator`] namespace: supervisor-installed and shared with
//! the effect journal in the daemon, self-minted in standalone use.

use crate::{PromptHistoryCompiler, StoreHandle, unix_time_ms};
use async_trait::async_trait;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, ChildReport, ChipState, ReportVerification};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
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
    AgentId, BranchId, CredentialAlias, DeviceId, EventId, ItemId, MenuId, NodeId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    ErrorRecoveryCardKind, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::provider::{
    AccountUsage, Block, CacheCostEstimate, CacheStatAvailability, FinishReason, NormalizedUsage,
    PROVIDER_OPAQUE_EXTENSION_KIND, PrefixDigests, StreamEvent, Usage, UsageRequestKind,
    UsageScope, WEB_SOURCES_EXTENSION_KIND, WebSource,
};
use haider_protocol::state::{RunState, WaitReason};
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_protocol::verify::VerifyVerdict;
use haider_provider::{
    Message, PromptCacheMetadata, Provider, ProviderError, ProviderErrorKind, ResolvedAttachment,
    ToolDefinition, TurnRequest, canonical_tool_definitions_digest,
};
use haider_tools::{RequestInput, TodoWrite};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN: usize = 32;
const DEFAULT_MAX_CONTINUATIONS_PER_TURN: usize = 8;
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
    /// Deterministic daemon-owned policy bound to every request in this actor.
    pub system_prompt: Option<String>,
    /// General tools the paired dispatcher can execute.
    pub tools: Vec<ToolDefinition>,
    /// CAS-backed attachments resolved before crossing the provider boundary.
    pub attachments: Vec<ResolvedAttachment>,
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
    /// Exclusive end of the latest active compaction-summary message.
    pub cache_compaction_summary_end: Option<usize>,
    /// Conservative future-read expectation for explicit cache resources.
    pub cache_expected_later_reads: u32,
    /// Observed gap in the current ephemeral cache domain.
    pub cache_reuse_gap_ms: Option<u64>,
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
    /// Injected backoff wait for the M4 provider-retry seam. Defaults to a
    /// real `tokio` timer; laws swap in an instant recording sleeper.
    pub retry_sleeper: Arc<dyn RetrySleeper>,
    /// Durable compaction implementation installed by the daemon. Standalone
    /// actors surface context overflow when none is configured.
    pub context_compactor: Option<Arc<dyn ContextCompactor>>,
    pub command_capacity: usize,
    pub broadcast_capacity: usize,
    /// Hard ceiling on provider requests made by one logical turn.
    pub max_provider_requests_per_turn: usize,
    /// Independent guard against providers repeatedly exhausting output.
    pub max_continuations_per_turn: usize,
    /// Maximum number of submissions parked behind the active turn.
    pub deferred_command_capacity: usize,
    /// Daemon supervisors close/reconcile their effect broker before writing
    /// `Cancelled`. Standalone actors retain the direct terminal commit.
    pub supervisor_commits_cancelled: bool,
    /// Optional supervisor-owned event namespace shared by every turn actor
    /// and effect journal in one worker generation.
    event_ids: Option<Arc<EventIdGenerator>>,
    started_at_ms: Option<u64>,
}

impl HarnessConfig {
    /// Convenience constructor with v0 defaults (fake model, small channels).
    pub fn for_session(
        session_id: SessionId,
        device_id: DeviceId,
        authority_epoch: u64,
        worker_generation: u64,
    ) -> Self {
        Self {
            session_id,
            branch_id: None,
            agent_id: None,
            device_id,
            authority_epoch,
            worker_generation,
            model: "fake-model".into(),
            max_tokens: 4096,
            context_window: None,
            reserved_output_tokens: 4096,
            cached_input_is_subset: true,
            context_compaction_v1: false,
            system_prompt: None,
            tools: Vec::new(),
            attachments: Vec::new(),
            usage_account: None,
            usage_scope: UsageScope::default(),
            cache_stable_history_end: None,
            cache_current_user_start: None,
            cache_compaction_summary_end: None,
            cache_expected_later_reads: 0,
            cache_reuse_gap_ms: None,
            reasoning_settings: String::new(),
            initial_rotation: None,
            rotation_budget_consumed: false,
            provider_attempt_resolver: None,
            retry_sleeper: Arc::new(RealRetrySleeper),
            context_compactor: None,
            command_capacity: 8,
            broadcast_capacity: 128,
            max_provider_requests_per_turn: DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN,
            max_continuations_per_turn: DEFAULT_MAX_CONTINUATIONS_PER_TURN,
            deferred_command_capacity: DEFAULT_DEFERRED_COMMAND_CAPACITY,
            supervisor_commits_cancelled: false,
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

/// Durable `request_input` checkpoint reconstructed after daemon restart.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestInputCheckpoint {
    pub menu: Menu,
    pub request_seq: u64,
    pub opening_generation: u64,
    pub tool_item_id: ItemId,
    pub call_id: String,
    /// `request_input` for question checkpoints, or the mutating tool whose
    /// broker approval is waiting on the same durable menu CAS.
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
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitPartialStreamTurn {
    pub run_id: RunId,
    pub messages: Vec<Message>,
    pub checkpoint: PartialStreamCheckpoint,
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

/// A provider/account replacement for the current logical turn.
pub struct ResolvedProviderAttempt {
    pub provider: Arc<dyn Provider>,
    pub account: CredentialAlias,
    pub rotation: RotationEvent,
}

/// Result of consulting the daemon at an eligible pre-first-event failure.
pub enum ProviderAttemptDecision {
    /// Retry with refreshed credentials for the same account. This does not
    /// consume the account-rotation allowance.
    Retry {
        provider: Arc<dyn Provider>,
        account: CredentialAlias,
    },
    /// Commit the supplied durable event, then retry with the alternate.
    Rotate(ResolvedProviderAttempt),
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
    let sample = u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("eight bytes"));
    floor + sample % (base.saturating_sub(floor).saturating_add(1))
}

/// Two-phase compaction port. `plan` is journaled before `compact` performs
/// private summarization and commits the final immutable overlay node.
#[async_trait]
pub trait ContextCompactor: Send + Sync + std::fmt::Debug {
    async fn plan(
        &self,
        run_id: &RunId,
        resume_cause: CompactionResume,
    ) -> Result<CompactionIntent, HaiderError>;

    async fn compact(
        &self,
        run_id: &RunId,
        intent: &CompactionIntent,
        covered_messages: Vec<Message>,
    ) -> Result<Message, HaiderError>;
}

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    async fn execute(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError>;

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
    state: watch::Receiver<Option<RunState>>,
    committed_menus: watch::Sender<Option<RawEnvelope>>,
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
        if !serde_json::from_value::<EventPayload>(envelope.payload.clone())
            .is_ok_and(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
        {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "committed menu wake must carry MenuAnswered",
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

    pub fn current_state(&self) -> Option<RunState> {
        self.state.borrow().clone()
    }

    pub fn state_receiver(&self) -> watch::Receiver<Option<RunState>> {
        self.state.clone()
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
    Stop {
        completed: oneshot::Sender<()>,
    },
}

enum TurnSubmission {
    Local(SubmitTurn),
    Committed(SubmitCommittedTurn),
    Checkpoint(Box<SubmitCheckpointTurn>),
    PartialStream(Box<SubmitPartialStreamTurn>),
    ChildWait(Box<SubmitChildWaitTurn>),
}

enum MenuWake {
    Command(ActorCommand),
    Committed(RawEnvelope),
}

/// Single-session, single-writer run loop.
pub struct HarnessActor {
    config: HarnessConfig,
    provider: Arc<dyn Provider>,
    dispatcher: Option<Arc<dyn ToolDispatcher>>,
    store: Arc<dyn StoreHandle>,
    commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RawEnvelope>,
    state: watch::Sender<Option<RunState>>,
    committed_menus: watch::Receiver<Option<RawEnvelope>>,
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
        let started_at_ms = config.started_at_ms.unwrap_or_else(unix_time_ms);
        let event_ids = config.event_ids.clone().unwrap_or_else(|| {
            Arc::new(EventIdGenerator::new(format!(
                "evt-{}-{}-{}",
                config.session_id, config.worker_generation, started_at_ms
            )))
        });
        let (command_sender, commands) = mpsc::channel(config.command_capacity.max(1));
        let (events, _) = broadcast::channel(config.broadcast_capacity.max(1));
        let (state, state_receiver) = watch::channel(None);
        let (committed_menus, committed_menu_receiver) = watch::channel(None);
        let handle = HarnessHandle {
            commands: command_sender,
            events: events.clone(),
            state: state_receiver,
            committed_menus,
        };
        (
            Self {
                config,
                provider,
                dispatcher,
                store,
                commands,
                events,
                state,
                committed_menus: committed_menu_receiver,
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
            },
            handle,
        )
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
                    let outcome = self.drive_turn(request, cancel).await;
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
                ActorCommand::Stop { completed } => {
                    let _ = completed.send(());
                    break;
                }
            }
        }
    }

    /// Runs one turn to a terminal state. Every return path commits that
    /// terminal `RunState` (best effort) before reporting the outcome.
    async fn drive_turn(&mut self, submit: TurnSubmission, cancel: CancelToken) -> TurnOutcome {
        // A subturn belongs only to the active logical turn. Cancellation or
        // failure may end that turn before a boundary; never leak its input
        // into a later queued turn.
        self.pending_subturns.clear();
        let (run_id, mut messages, checkpoint, partial_stream, child_wait) = match submit {
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
                )
            }
            TurnSubmission::Committed(submit) => (submit.run_id, submit.messages, None, None, None),
            TurnSubmission::Checkpoint(submit) => {
                let submit = *submit;
                (
                    submit.run_id,
                    submit.messages,
                    Some(submit.checkpoint),
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
                )
            }
            TurnSubmission::ChildWait(submit) => {
                let submit = *submit;
                (
                    submit.run_id,
                    submit.messages,
                    None,
                    None,
                    Some(submit.checkpoint),
                )
            }
        };
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

        let mut message: Option<TextAccumulator> = None;
        let mut reasoning: Option<TextAccumulator> = None;
        let mut tools: Vec<ToolAccumulator> = Vec::new();
        let mut deferred = Vec::<DeferredAccumulator>::new();
        if let Some(checkpoint) = checkpoint {
            tools.push(ToolAccumulator {
                item_id: checkpoint.tool_item_id,
                call_id: checkpoint.call_id.clone(),
                name: checkpoint.tool_name.clone(),
                args: checkpoint.args.clone(),
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
            let resumed = if checkpoint.tool_name == "request_input" {
                self.resume_request_input(&run_id, &mut tools, 0, &cancel, checkpoint.menu)
                    .await
            } else {
                self.resume_tool_approval(&run_id, &mut tools, 0, &cancel, checkpoint.menu)
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
                .wait_for_error_recovery_answer(&run_id, &cancel, &checkpoint.menu)
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
        if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await {
            return self.errored_state_outcome(&run_id, error).await;
        }
        let mut provider = Arc::clone(&self.provider);
        let mut usage_account = self.config.usage_account.clone();
        let mut rotation_budget_consumed = self.config.rotation_budget_consumed;
        if let Some(initial_rotation) = self.config.initial_rotation.take() {
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
        let mut provider_request_count = 0usize;
        let mut continuation_count = 0usize;
        let mut forced_compaction_used = false;
        let mut provider_attempt = 0usize;
        let mut completed_usage: Option<Usage> = None;
        // W-B: provider-executed tool rows and cited web sources are
        // TURN-scoped — a pause_turn boundary can split a server call from
        // its result across requests, and the bounded sources list journals
        // exactly once, under the finished message.
        let mut server_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        let mut web_sources: Vec<WebSource> = Vec::new();

        'requests: loop {
            messages.extend(
                std::mem::take(&mut self.pending_nudges)
                    .into_iter()
                    .map(Message::user_text),
            );
            let request_projection_compacted = match self
                .enforce_context_policy(&run_id, &mut messages, current_turn_start)
                .await
            {
                Ok(compacted) => compacted,
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
            if request_projection_compacted {
                // Compaction replaces the covered prefix with exactly one
                // immutable summary message; the current-turn suffix stays
                // verbatim after it.
                stable_history_end = usize::from(!messages.is_empty());
                current_turn_start = stable_history_end;
                latest_compaction_summary_end = Some(stable_history_end);
            }
            if provider_attempt == 0 {
                provider_request_count = provider_request_count.saturating_add(1);
            }
            if provider_request_count > self.config.max_provider_requests_per_turn {
                return self
                    .errored_outcome_with_items(
                        &run_id,
                        &mut message,
                        &mut reasoning,
                        &mut tools,
                        loop_limit_error(
                            provider_request_count,
                            self.config.max_provider_requests_per_turn,
                        ),
                    )
                    .await;
            }
            provider_attempt = provider_attempt.saturating_add(1);
            let mut prefix_digests = usage_prefix_digests(
                &self.config,
                &messages[..stable_history_end.min(messages.len())],
            );
            let mut cache_metadata = prompt_cache_metadata(
                &self.config,
                &messages,
                stable_history_end,
                current_turn_start,
                latest_compaction_summary_end,
                prefix_digests.clone(),
                usage_account.as_ref(),
            );
            let mut provider_request = TurnRequest {
                messages: messages.clone(),
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system_prompt: self.config.system_prompt.clone(),
                tools: self.config.tools.clone(),
                attachments: self.config.attachments.clone(),
                cache_metadata: Some(cache_metadata.clone()),
            };
            if let Some(rendered) = provider.rendered_cache_prefix_digests(&provider_request) {
                prefix_digests = rendered;
                cache_metadata = prompt_cache_metadata(
                    &self.config,
                    &messages,
                    stable_history_end,
                    current_turn_start,
                    latest_compaction_summary_end,
                    prefix_digests.clone(),
                    usage_account.as_ref(),
                );
                provider_request.cache_metadata = Some(cache_metadata.clone());
            }
            let mut request_usage: Option<Usage> = None;
            let attempt_provider = Arc::clone(&provider);
            let mut opening = Box::pin(attempt_provider.stream_turn(provider_request));
            let mut stream = loop {
                let opened = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return self
                            .cancelled_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                            )
                            .await;
                    }
                    opened = &mut opening => opened,
                    command = self.commands.recv() => {
                        let Some(command) = command else {
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
                            let outcome = self
                                .cancelled_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                )
                                .await;
                            let _ = completed.send(());
                            return outcome;
                        }
                        self.service_command_without_menu(command);
                        continue;
                    }
                };
                if cancel.is_cancelled() {
                    return self
                        .cancelled_outcome_with_items(
                            &run_id,
                            &mut message,
                            &mut reasoning,
                            &mut tools,
                        )
                        .await;
                }
                match opened {
                    Ok(stream) => break stream,
                    Err(error) if error.kind == ProviderErrorKind::ContextExceeded => {
                        let compacted = if request_projection_compacted {
                            Err(repeated_context_overflow_after_compaction())
                        } else {
                            self.force_context_compaction(
                                &run_id,
                                &mut messages,
                                current_turn_start,
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
                        provider_attempt = 0;
                        continue 'requests;
                    }
                    Err(error) => {
                        if let Err(error) = self
                            .prepare_pre_first_event_retry(
                                ProviderRetryContext {
                                    run_id: &run_id,
                                    cancel: &cancel,
                                },
                                provider_attempt,
                                &mut provider,
                                &mut usage_account,
                                &mut rotation_budget_consumed,
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
                return self.errored_state_outcome(&run_id, error).await;
            }

            let mut assistant_blocks = Vec::new();
            let mut tool_results = Vec::new();
            let mut provider_content_seen = false;
            let mut refusal_reason = String::new();
            loop {
                let next = tokio::select! {
                    // Cancellation owns ties. Provider progress is polled
                    // before command service on every round so an unbounded
                    // command arrival rate cannot starve the active stream.
                    biased;
                    () = cancel.cancelled() => {
                        return self
                            .cancelled_outcome_with_items(
                                &run_id,
                                &mut message,
                                &mut reasoning,
                                &mut tools,
                            )
                            .await;
                    }
                    item = stream.recv() => item,
                    command = self.commands.recv() => {
                        let Some(command) = command else {
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
                            let outcome = self
                                .cancelled_outcome_with_items(
                                    &run_id,
                                    &mut message,
                                    &mut reasoning,
                                    &mut tools,
                                )
                                .await;
                            let _ = completed.send(());
                            return outcome;
                        }
                        self.service_command_without_menu(command);
                        continue;
                    }
                };

                if cancel.is_cancelled() {
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
                        if !matches!(event, StreamEvent::UsageUpdate(_)) {
                            provider_content_seen = true;
                        }
                        event
                    }
                    Err(error)
                        if error.kind == ProviderErrorKind::ContextExceeded
                            && !provider_content_seen =>
                    {
                        let compacted = if request_projection_compacted {
                            Err(repeated_context_overflow_after_compaction())
                        } else {
                            self.force_context_compaction(
                                &run_id,
                                &mut messages,
                                current_turn_start,
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
                        provider_attempt = 0;
                        continue 'requests;
                    }
                    Err(error) if !provider_content_seen => {
                        if let Err(error) = self
                            .prepare_pre_first_event_retry(
                                ProviderRetryContext {
                                    run_id: &run_id,
                                    cancel: &cancel,
                                },
                                provider_attempt,
                                &mut provider,
                                &mut usage_account,
                                &mut rotation_budget_consumed,
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
                        if message
                            .as_ref()
                            .is_some_and(|partial| !partial.text.is_empty())
                        {
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
                            let menu = recovery_menu(
                                self.next_menu_id(),
                                &run_id,
                                Some(source_item),
                                ErrorRecoveryCardKind::PartialStream,
                                presentation,
                                Some(self.config.usage_scope.provider.clone()),
                                usage_account.clone(),
                                true,
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
                            if let Err(error) = self
                                .commit_state(
                                    &run_id,
                                    RunState::InputRequired {
                                        menu: menu.id.clone(),
                                    },
                                )
                                .await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            let action = match self
                                .wait_for_error_recovery_answer(&run_id, &cancel, &menu)
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
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
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
                                let outcome = self
                                    .cancelled_outcome_with_items(
                                        &run_id,
                                        &mut message,
                                        &mut reasoning,
                                        &mut tools,
                                    )
                                    .await;
                                let _ = completed.send(());
                                return outcome;
                            }
                            Ok(command) => self.service_command_without_menu(command),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    }
                }

                let event_result: Result<Option<Message>, DriveError> = match event {
                    StreamEvent::TextDelta { text } => {
                        assistant_blocks.push(Block::Text { text: text.clone() });
                        self.apply_text_delta(&run_id, &mut message, text, false)
                            .await
                            .map(|()| None)
                    }
                    StreamEvent::ReasoningDelta { text } => {
                        // Normalized reasoning has no provider-valid signature
                        // and must never be replayed into a follow-up request.
                        self.apply_text_delta(&run_id, &mut reasoning, text, true)
                            .await
                            .map(|()| None)
                    }
                    StreamEvent::RefusalDelta { text } => {
                        // Refusal content has its own provider channel. The
                        // terminal Refusal outcome survives, but this content
                        // must never become assistant text or prompt history.
                        append_bounded_refusal(&mut refusal_reason, &text);
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
                                    if let Err(error) =
                                        self.commit_state(&run_id, RunState::Thinking).await
                                    {
                                        return self.errored_state_outcome(&run_id, error).await;
                                    }
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
                            self.commit_server_tool_row(&run_id, &call_id, name, args, status)
                                .await?;
                            self.commit_payload(
                                &run_id,
                                EventPayload::ToolResult {
                                    call_id,
                                    result: BoundedResult {
                                        preview,
                                        truncated: false,
                                        artifact: None,
                                        cursor: None,
                                        status: if is_error {
                                            ToolResultStatus::Failed
                                        } else {
                                            ToolResultStatus::Completed
                                        },
                                        reason: is_error
                                            .then(|| "server tool reported an error".into()),
                                        presentation: is_error.then(|| {
                                            tool_error_presentation(
                                                "server-tool-failed",
                                                "Provider tool failed",
                                                "The provider-hosted tool reported an error.",
                                            )
                                        }),
                                    },
                                },
                                prompt_omit_render(),
                            )
                            .await
                            .map_err(DriveError::Store)?;
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
                            prefix_digests.clone(),
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
                        let footprint =
                            context_footprint_from_usage(&self.config, &usage, &messages);
                        request_usage = Some(usage.clone());
                        match cumulative_usage(completed_usage.as_ref(), &usage) {
                            Ok(usage) => {
                                async {
                                    if self.config.context_compaction_v1 {
                                        self.commit_context_footprint(&run_id, &footprint)
                                            .await
                                            .map_err(DriveError::Store)?;
                                    }
                                    self.commit_payload(
                                        &run_id,
                                        EventPayload::Usage(usage),
                                        prompt_omit_render(),
                                    )
                                    .await
                                    .map_err(DriveError::Store)?;
                                    Ok(None)
                                }
                                .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    StreamEvent::Finish { reason } => {
                        if reason == FinishReason::Cancelled {
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
                        if let Err(error) = self.complete_text(&run_id, &mut message, false).await {
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
                        if let Err(error) = self.complete_text(&run_id, &mut reasoning, true).await
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
                        if !assistant_blocks.is_empty() {
                            messages
                                .push(Message::assistant(std::mem::take(&mut assistant_blocks)));
                        }
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
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
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
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
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
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
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
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
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
                if cancel.is_cancelled() {
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

    /// Opens the text/reasoning item on its first delta, then commits the delta.
    async fn apply_text_delta(
        &mut self,
        run_id: &RunId,
        accumulator: &mut Option<TextAccumulator>,
        text: String,
        reasoning: bool,
    ) -> Result<(), DriveError> {
        let open = match accumulator.take() {
            Some(open) => open,
            None => {
                let item_id = self.next_item_id();
                let empty = if reasoning {
                    TurnItem::Reasoning {
                        summary: String::new(),
                    }
                } else {
                    TurnItem::AgentMessage {
                        text: String::new(),
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
                TextAccumulator {
                    item_id,
                    text: String::new(),
                }
            }
        };

        let active = accumulator.insert(open);
        active.text.push_str(&text);
        let delta = if reasoning {
            ItemDelta::Reasoning { text }
        } else {
            ItemDelta::Text { text }
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
        Ok(())
    }

    async fn prepare_pre_first_event_retry(
        &mut self,
        context: ProviderRetryContext<'_>,
        provider_attempt: usize,
        provider: &mut Arc<dyn Provider>,
        account: &mut Option<CredentialAlias>,
        rotation_budget_consumed: &mut bool,
        error: ProviderError,
    ) -> Result<(), DriveError> {
        if !*rotation_budget_consumed
            && provider_error_allows_rotation(&error)
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
                    if provider_attempt < MAX_API_RETRIES {
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
                ProviderAttemptDecision::Wait => {
                    *rotation_budget_consumed = true;
                }
                ProviderAttemptDecision::Stop => {
                    *rotation_budget_consumed = true;
                    return Err(DriveError::Provider(error));
                }
            }
        }
        if provider_error_allows_retry(&error) && provider_attempt < MAX_API_RETRIES {
            self.wait_before_provider_retry(
                context.run_id,
                context.cancel,
                provider_attempt,
                &error,
            )
            .await
        } else {
            Err(DriveError::Provider(error))
        }
    }

    async fn force_context_compaction(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        current_turn_start: usize,
        forced_compaction_used: &mut bool,
    ) -> Result<(), DriveError> {
        if *forced_compaction_used {
            return Err(repeated_context_overflow_after_compaction());
        }
        self.perform_context_compaction(run_id, messages, current_turn_start)
            .await?;
        *forced_compaction_used = true;
        Ok(())
    }

    async fn perform_context_compaction(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        current_turn_start: usize,
    ) -> Result<(), DriveError> {
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
        let intent = compactor
            .plan(run_id, CompactionResume::AutoMidTurn)
            .await
            .map_err(DriveError::Store)?;
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

        let suffix = messages.split_off(current_turn_start);
        let covered = std::mem::take(messages);
        let summary = compactor
            .compact(run_id, &intent, covered)
            .await
            .map_err(DriveError::Store)?;
        messages.push(summary);
        messages.extend(suffix);
        // The daemon committed a new compaction node behind this actor's
        // cached parent. Reload before the next current-run node is appended
        // so later output descends from the projection switch.
        self.tree_head = None;
        self.tree_head_initialized = false;
        // The compactor atomically commits its final overlay/item together
        // with this resumed state; mirror that durable fact into the watch.
        self.state.send_replace(Some(RunState::Thinking));
        Ok(())
    }

    /// Publishes request occupancy and enforces the daemon-pinned soft/hard
    /// context policy immediately before every provider request. Unknown
    /// catalog windows publish honest estimates but disable proactive
    /// compaction; provider-reported overflow can still force recovery.
    async fn enforce_context_policy(
        &mut self,
        run_id: &RunId,
        messages: &mut Vec<Message>,
        current_turn_start: usize,
    ) -> Result<bool, DriveError> {
        let before = estimated_context_footprint(&self.config, messages);
        if self.config.context_compaction_v1 {
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
        self.perform_context_compaction(run_id, messages, current_turn_start)
            .await?;
        let after = estimated_context_footprint(&self.config, messages);
        if self.config.context_compaction_v1 {
            self.commit_context_footprint(run_id, &after)
                .await
                .map_err(DriveError::Store)?;
        }
        if after.used_tokens > input_budget {
            return Err(DriveError::Provider(ProviderError::new(
                ProviderErrorKind::ContextExceeded,
                format!(
                    "compacted provider input estimate {} exceeds budget {input_budget}",
                    after.used_tokens
                ),
            )));
        }
        Ok(true)
    }

    /// Commits `Retrying { attempt, max, delay_ms, reason }` (W-C M4: the
    /// visible `attempt K/max` counter), waits through the injected sleeper,
    /// then commits `Thinking` — the R6 backoff between provider attempts.
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
                presentation: error.presentation.clone(),
            };
            return Err(DriveError::Provider(capped));
        }
        let reason = if error.kind == ProviderErrorKind::RateLimited {
            WaitReason::RateLimit
        } else {
            WaitReason::ProviderBackoff
        };
        // `retry_after_ms` (429/529 Retry-After) OVERRIDES the computed
        // backoff; otherwise use the jittered exponential schedule.
        let delay_ms = error
            .retry_after_ms
            .unwrap_or_else(|| retry_jittered_backoff_ms(run_id, failed_attempt));
        self.commit_state(
            run_id,
            RunState::Retrying {
                attempt: u32::try_from(failed_attempt.saturating_add(1)).unwrap_or(u32::MAX),
                max: u32::try_from(MAX_API_RETRIES).unwrap_or(u32::MAX),
                delay_ms,
                reason,
            },
        )
        .await
        .map_err(DriveError::Store)?;
        // Clone the sleeper Arc into a local so the pinned backoff future
        // borrows IT, not `self` — the loop below services commands through
        // `&mut self` while the same sleep deadline is still pending.
        let sleeper = Arc::clone(&self.config.retry_sleeper);
        let sleep = sleeper.sleep(delay_ms);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(DriveError::Cancelled),
                // L1: a Stop (or a closed command channel) during a long
                // Retry-After backoff must not block shutdown for the full
                // delay — treat it as a cancel. Other commands are serviced and
                // the SAME sleep deadline is re-awaited, so the backoff clock
                // never restarts.
                command = self.commands.recv() => {
                    match command {
                        None => return Err(DriveError::Cancelled),
                        Some(ActorCommand::Stop { completed }) => {
                            cancel.cancel();
                            let _ = completed.send(());
                            return Err(DriveError::Cancelled);
                        }
                        Some(other) => {
                            self.service_command_without_menu(other);
                            continue;
                        }
                    }
                }
                () = &mut sleep => break,
            }
        }
        if cancel.is_cancelled() {
            return Err(DriveError::Cancelled);
        }
        self.commit_state(run_id, RunState::Thinking)
            .await
            .map_err(DriveError::Store)
    }

    /// Commits `Completed` with the accumulated text; no-op when nothing streamed.
    async fn complete_text(
        &mut self,
        run_id: &RunId,
        accumulator: &mut Option<TextAccumulator>,
        reasoning: bool,
    ) -> Result<(), DriveError> {
        let Some(active) = accumulator.as_ref() else {
            return Ok(());
        };
        let item = if reasoning {
            TurnItem::Reasoning {
                summary: active.text.clone(),
            }
        } else {
            TurnItem::AgentMessage {
                text: active.text.clone(),
            }
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
    ) -> Result<(ItemId, String), DriveError> {
        let active = accumulator.take().ok_or_else(|| {
            DriveError::Provider(provider_protocol_error(
                "partial-stream recovery had no active assistant item",
            ))
        })?;
        let item_id = active.item_id.clone();
        let text = active.text;
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
        Ok((item_id, text))
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
        // A delegated child may only invoke declarations present in its
        // resolved grant-filtered pack. This actor-side fence covers the two
        // actor-owned tools before the daemon dispatcher is reached.
        if self.config.agent_id.is_some()
            && !self
                .config
                .tools
                .iter()
                .any(|definition| definition.name == tools[index].name)
        {
            let reason = format!(
                "grant ceiling violation: child is not allowed to use `{}`",
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
                artifact: None,
                cursor: None,
                status: ToolResultStatus::Rejected,
                reason: Some(reason),
                presentation: Some(tool_error_presentation(
                    "grant-ceiling-violation",
                    "Tool grant denied",
                    "This child is not allowed to use the requested tool.",
                )),
            };
            let call_id = tools[index].call_id.clone();
            self.commit_payload(
                run_id,
                EventPayload::ToolResult {
                    call_id: call_id.clone(),
                    result: result.clone(),
                },
                prompt_verbatim_render(),
            )
            .await
            .map_err(DriveError::Store)?;
            self.commit_tool_completed(run_id, &tools[index], ToolStatus::Rejected)
                .await?;
            tools.remove(index);
            return Ok(Some(Message::tool_result(call_id, result.preview, false)));
        }
        if tools[index].name == "request_input" {
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
            let call_id = tools[index].call_id.clone();
            self.commit_payload(
                run_id,
                EventPayload::ToolResult {
                    call_id: call_id.clone(),
                    result: result.clone(),
                },
                prompt_verbatim_render(),
            )
            .await
            .map_err(DriveError::Store)?;
            self.commit_tool_completed(run_id, &tools[index], result.status.item_status())
                .await?;
            tools.remove(index);
            self.commit_state(run_id, RunState::Streaming)
                .await
                .map_err(DriveError::Store)?;
            return Ok(Some(Message::tool_result(
                call_id,
                result.preview,
                result.truncated,
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
        args: serde_json::Value,
        cancel: &CancelToken,
        dispatcher: &Arc<dyn ToolDispatcher>,
    ) -> Result<GeneralToolOutcome, DriveError> {
        loop {
            let execution = dispatcher.execute(
                run_id,
                &tool.item_id,
                &tool.call_id,
                &tool.name,
                args.clone(),
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
                let _ = dispatcher.close().await;
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
                    self.commit_payload(
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
                MenuWake::Command(command @ ActorCommand::Nudge { .. }) => {
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
                    let payload = serde_json::from_value::<EventPayload>(envelope.payload)
                        .map_err(|error| {
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
        let result = match TodoWrite::from_tool_args(args) {
            Ok(request) => {
                self.emit_plan_facts(run_id, &request).await?;
                BoundedResult {
                    preview: request.result_echo().to_string(),
                    truncated: false,
                    artifact: None,
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
                artifact: None,
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
        let call_id = tools[index].call_id.clone();
        self.commit_payload(
            run_id,
            EventPayload::ToolResult {
                call_id: call_id.clone(),
                result: result.clone(),
            },
            prompt_verbatim_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        self.commit_tool_completed(run_id, &tools[index], result.status.item_status())
            .await?;
        tools.remove(index);
        Ok(Message::tool_result(
            call_id,
            result.preview,
            result.truncated,
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
        let request = RequestInput::from_tool_args(args).map_err(tool_error_to_drive)?;
        let menu = request.menu(self.next_menu_id());
        self.commit_payload(
            run_id,
            EventPayload::MenuOpened(menu.clone()),
            prompt_omit_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        self.commit_state(
            run_id,
            RunState::InputRequired {
                menu: menu.id.clone(),
            },
        )
        .await
        .map_err(DriveError::Store)?;

        self.wait_for_request_input(run_id, tools, index, cancel, request, menu)
            .await
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
        let request = RequestInput::from_tool_args(args).map_err(tool_error_to_drive)?;
        self.wait_for_request_input(run_id, tools, index, cancel, request, menu)
            .await
    }

    async fn resume_tool_approval(
        &mut self,
        run_id: &RunId,
        tools: &mut Vec<ToolAccumulator>,
        index: usize,
        cancel: &CancelToken,
        menu: Menu,
    ) -> Result<Message, DriveError> {
        let Some(dispatcher) = self.dispatcher.as_ref().map(Arc::clone) else {
            return Err(DriveError::Store(HaiderError::new(
                ErrorCode::Internal,
                "recovered permission checkpoint has no tool dispatcher",
                false,
            )));
        };
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
        let call_id = tools[index].call_id.clone();
        self.commit_payload(
            run_id,
            EventPayload::ToolResult {
                call_id: call_id.clone(),
                result: result.clone(),
            },
            prompt_verbatim_render(),
        )
        .await
        .map_err(DriveError::Store)?;
        self.commit_tool_completed(run_id, &tools[index], result.status.item_status())
            .await?;
        tools.remove(index);
        self.commit_state(run_id, RunState::Streaming)
            .await
            .map_err(DriveError::Store)?;
        Ok(Message::tool_result(
            call_id,
            result.preview,
            result.truncated,
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
                MenuWake::Command(command @ ActorCommand::Nudge { .. }) => {
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
                    let payload = serde_json::from_value::<EventPayload>(envelope.payload)
                        .map_err(|error| {
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
                MenuWake::Command(command @ ActorCommand::Nudge { .. }) => {
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
                    let payload = serde_json::from_value::<EventPayload>(envelope.payload)
                        .map_err(|error| {
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
            if let Err(error) = self
                .commit_payload(
                    run_id,
                    EventPayload::ToolResult {
                        call_id: call_id.clone(),
                        result: BoundedResult {
                            preview: result.clone(),
                            truncated: false,
                            artifact: None,
                            cursor: None,
                            status: ToolResultStatus::Completed,
                            reason: None,
                            presentation: None,
                        },
                    },
                    prompt_verbatim_render(),
                )
                .await
            {
                if let Some(completed) = completed {
                    let _ = completed.send(Err(error.clone()));
                }
                return Err(DriveError::Store(error));
            }
            self.commit_tool_completed(run_id, &tools[index], ToolStatus::Completed)
                .await?;
            tools.remove(index);
            self.commit_state(run_id, RunState::Streaming)
                .await
                .map_err(DriveError::Store)?;
            if let Some(completed) = completed {
                let _ = completed.send(Ok(()));
            }
            return Ok(Message::tool_result(call_id, result, false));
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
                artifact: None,
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
            if !pending.item_completed {
                self.commit_tool_completed(run_id, &tools[tool_index], result.status.item_status())
                    .await?;
                pending.item_completed = true;
            }
            tools.remove(tool_index);
            dispatcher
                .acknowledge_deferred(&pending.ticket)
                .await
                .map_err(DriveError::Store)?;
            results.push(Message::tool_result(
                pending.call_id,
                result.preview,
                result.truncated,
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
        let mut envelopes = [
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
        self.store
            .append(&mut envelopes)
            .await
            .map_err(DriveError::Store)?;
        for envelope in envelopes {
            let _ = self.events.send(envelope);
        }
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
        let mut envelopes = [
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
        self.store
            .append(&mut envelopes)
            .await
            .map_err(DriveError::Store)?;
        for envelope in envelopes {
            let _ = self.events.send(envelope);
        }
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
            parse_tool_args(tool)?
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
        if let Some(dispatcher) = self.dispatcher.as_ref()
            && let Err(error) = dispatcher.cancel_outstanding_deferred().await
        {
            return errored_outcome(error);
        }
        if let Err(error) = self
            .complete_open_items(run_id, message, reasoning, tools, ToolStatus::Cancelled)
            .await
        {
            return errored_outcome(drive_error_to_haider(error));
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
        let mut envelopes = [
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
        ];
        self.store.append(&mut envelopes).await?;
        for committed in envelopes {
            // No live subscribers is fine — the store already has the
            // complete failure terminal.
            let _ = self.events.send(committed);
        }
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

    async fn commit_item(
        &mut self,
        run_id: &RunId,
        item: ItemEvent,
    ) -> Result<RawEnvelope, HaiderError> {
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
            .await
        } else {
            self.commit_payload(run_id, EventPayload::Item(item), prompt_verbatim_render())
                .await
        }
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
                "data": data,
            }),
        };
        let render = hidden_prompt_verbatim_render();
        let parent = self.tree_parent().await.map_err(DriveError::Store)?;
        let node = TreeNode {
            node: self.next_node_id(),
            parent,
            kind: NodeKind::AssistantCommit {
                text: String::new(),
                verdict: VerifyVerdict::NotApplicable,
            },
        };
        let mut envelopes = [
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: item.clone(),
                }),
                render,
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed { item_id, item }),
                render,
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )
            .map_err(DriveError::Store)?,
        ];
        self.store
            .append(&mut envelopes)
            .await
            .map_err(DriveError::Store)?;
        for committed in envelopes {
            let _ = self.events.send(committed);
        }
        self.tree_head = Some(node.node);
        Ok(())
    }

    /// Atomically journals one PROVIDER-executed tool call as a closed,
    /// UI-visible row (W-B decision 6).
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
        let mut envelopes = [
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Started {
                    item_id: item_id.clone(),
                    item: started,
                }),
                render,
            )
            .map_err(DriveError::Store)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::Item(ItemEvent::Completed {
                    item_id,
                    item: completed,
                }),
                render,
            )
            .map_err(DriveError::Store)?,
        ];
        self.store
            .append(&mut envelopes)
            .await
            .map_err(DriveError::Store)?;
        for committed in envelopes {
            let _ = self.events.send(committed);
        }
        Ok(())
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
        let item = footprint.extension_item().map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("context footprint could not serialize: {error}"),
                false,
            )
        })?;
        let TurnItem::Extension { kind, data } = item else {
            unreachable!("context footprint always uses the extension carrier");
        };
        self.commit_ui_extension_marker(run_id, &kind, data).await
    }

    async fn commit_extension_marker(
        &mut self,
        run_id: &RunId,
        kind: &str,
        data: serde_json::Value,
        render: RenderTargets,
    ) -> Result<(), HaiderError> {
        let item_id = self.next_item_id();
        let item = TurnItem::Extension {
            kind: kind.to_owned(),
            data,
        };
        let mut envelopes = [
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
        ];
        self.store.append(&mut envelopes).await?;
        for committed in envelopes {
            let _ = self.events.send(committed);
        }
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
        let node = TreeNode {
            node: self.next_node_id(),
            parent: self.tree_parent().await?,
            kind,
        };
        let mut envelopes = [
            self.uncommitted_envelope(run_id, payload, render)?,
            self.uncommitted_envelope(
                run_id,
                EventPayload::NodeCommitted(node.clone()),
                prompt_omit_render(),
            )?,
        ];
        self.store.append(&mut envelopes).await?;
        for committed in &envelopes {
            let _ = self.events.send(committed.clone());
        }
        self.tree_head = Some(node.node);
        Ok(envelopes[0].clone())
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
        let mut envelopes = [self.uncommitted_envelope(run_id, payload, render)?];
        self.store.append(&mut envelopes).await?;
        let [committed] = envelopes;
        // No live subscribers is fine — the store already has the envelope.
        let _ = self.events.send(committed.clone());
        Ok(committed)
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
        let payload = serde_json::to_value(payload).map_err(|error| {
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

/// Turn-loop failure, tagged by which port failed (drives the error surface).
#[derive(Debug)]
enum DriveError {
    Provider(ProviderError),
    Account(HaiderError),
    Store(HaiderError),
    Cancelled,
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
    text: String,
}

/// One in-flight tool call; `args` collects the streamed JSON fragments.
#[derive(Debug)]
struct ToolAccumulator {
    item_id: ItemId,
    call_id: String,
    name: String,
    args: String,
}

enum GeneralToolOutcome {
    Completed(BoundedResult),
    Deferred(Box<DeferredTicket>),
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

fn parse_tool_args(tool: &ToolAccumulator) -> Result<serde_json::Value, DriveError> {
    if tool.args.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&tool.args).map_err(|error| {
        DriveError::Provider(provider_protocol_error(format!(
            "tool call `{}` ended with malformed JSON arguments: {error}",
            tool.call_id,
        )))
    })
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
        args: parse_tool_args(tool)?,
    })
}

fn tool_args_or_raw(tool: &ToolAccumulator) -> serde_json::Value {
    if tool.args.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(&tool.args)
        .unwrap_or_else(|_| serde_json::Value::String(tool.args.clone()))
}

fn provider_protocol_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedFrame, message)
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

fn loop_limit_error(count: usize, limit: usize) -> HaiderError {
    let mut error = HaiderError::new(
        ErrorCode::LoopLimit,
        format!("provider request loop limit exceeded at request {count} (limit {limit})"),
        false,
    );
    error.details = Some(serde_json::json!({
        "provider_request_count": count,
        "provider_request_limit": limit,
    }));
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

fn provider_error_to_haider(provider_error: ProviderError) -> HaiderError {
    let mut error = HaiderError::new(
        ErrorCode::ProviderError,
        provider_error.to_string(),
        provider_error.retryable,
    );
    error.details = Some(serde_json::json!({
        "provider_error_kind": format!("{:?}", provider_error.kind),
        "retry_after_ms": provider_error.retry_after_ms,
    }));
    error.presentation = Some(provider_error.presentation);
    error
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
    copy_provider_metadata(&mut specialized, &error.presentation);
    error.presentation = specialized;
}

fn stream_interruption_presentation(error: &ProviderError) -> ErrorPresentation {
    let mut presentation = ErrorPresentation::new(
        "stream-interrupted",
        "Response stream interrupted",
        "The provider connection ended after part of the response was received.",
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

fn copy_provider_metadata(target: &mut ErrorPresentation, source: &ErrorPresentation) {
    target.provider_http_status = source.provider_http_status;
    target
        .provider_request_id
        .clone_from(&source.provider_request_id);
    target.retry_after_ms = source.retry_after_ms;
    target.reset_at_ms = source.reset_at_ms;
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

fn provider_error_allows_retry(error: &ProviderError) -> bool {
    error.retryable
        && matches!(
            error.kind,
            ProviderErrorKind::Transport
                | ProviderErrorKind::StreamInterrupted
                | ProviderErrorKind::RateLimited
                | ProviderErrorKind::Overloaded
        )
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
        | ProviderErrorKind::Transport
        | ProviderErrorKind::MalformedFrame
        | ProviderErrorKind::InvalidUtf8
        | ProviderErrorKind::Internal
        | ProviderErrorKind::QuotaExhausted
        | ProviderErrorKind::StreamInterrupted
        | ProviderErrorKind::ConnectionConfiguration => false,
    }
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
        tools: canonical_tool_definitions_digest(&config.tools),
        immutable_history: digest_json(&immutable_history),
        model: digest_json(&config.model),
        auth_mode: digest_json(&config.usage_scope.auth_scope),
        reasoning_settings: digest_json(&config.reasoning_settings),
    }
}

fn prompt_cache_metadata(
    config: &HarnessConfig,
    messages: &[Message],
    stable_history_end: usize,
    current_user_start: usize,
    latest_compaction_summary_end: Option<usize>,
    prefix_digests: PrefixDigests,
    account_scope: Option<&CredentialAlias>,
) -> PromptCacheMetadata {
    let stable_history_end = stable_history_end.min(messages.len());
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
        "account_scope": account_scope,
        "system_digest": prefix_digests.system,
        "tool_digest": prefix_digests.tools,
        "auth_digest": prefix_digests.auth_mode,
        "reasoning_digest": prefix_digests.reasoning_settings,
        "compaction_epoch": compaction_epoch,
    }));
    let stable_prefix_tokens = estimate_provider_request_input_tokens(
        &messages[..stable_history_end],
        &config.system_prompt,
        &config.tools,
        &[],
    );
    PromptCacheMetadata {
        stable_history_end,
        current_user_start,
        latest_compaction_summary_end,
        prefix_digests,
        cache_epoch,
        compaction_epoch,
        provider: config.usage_scope.provider.clone(),
        session_scope: config.session_id.as_str().to_owned(),
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
    prefix_digests: PrefixDigests,
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
    scope.prefix_digests = Some(prefix_digests);
    usage.cache_cost = usage.normalized.as_ref().and_then(|normalized| {
        haider_provider::estimate_cache_input_costs(&config.model, normalized)
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
    let hard_fit = window.checked_sub(reserved_output_tokens)?;
    let eighty_five_percent =
        u64::try_from(u128::from(window).saturating_mul(85) / 100).unwrap_or(u64::MAX);
    Some(eighty_five_percent.min(hard_fit))
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
    }
}

/// Conservative provider-neutral compiled-request accounting used when no
/// provider-reported request-local usage is available. It remains separate
/// from cumulative billing usage.
fn estimated_request_input_tokens(config: &HarnessConfig, messages: &[Message]) -> u64 {
    estimate_provider_request_input_tokens(
        messages,
        &config.system_prompt,
        &config.tools,
        &config.attachments,
    )
}

/// Deterministic accounting for the complete provider-bound request context.
/// Daemon-owned idle operations use the same estimator as live actor rounds.
/// Image bytes are intentionally excluded: providers tokenize vision inputs
/// independently of their base64 transport encoding.
pub const VISION_IMAGE_ESTIMATE_TOKENS: u64 = 1_600;

#[must_use]
pub fn estimate_provider_request_input_tokens(
    messages: &[Message],
    system_prompt: &Option<String>,
    tools: &[ToolDefinition],
    _attachments: &[ResolvedAttachment],
) -> u64 {
    let bytes = serde_json::to_vec(&(messages, system_prompt, tools))
        .map(|encoded| u64::try_from(encoded.len()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX);
    let image_count = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| {
            matches!(
                block,
                haider_protocol::provider::Block::Attachment(
                    haider_protocol::tool::AttachmentBlock::Image { .. }
                )
            )
        })
        .count();
    let image_tokens = u64::try_from(image_count)
        .unwrap_or(u64::MAX)
        .saturating_mul(VISION_IMAGE_ESTIMATE_TOKENS);
    (bytes.saturating_add(3) / 4).saturating_add(image_tokens)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod usage_tests {
    use super::*;
    use haider_protocol::provider::UsageSource;

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
        };
        let cumulative = cumulative_usage(Some(&completed), &current).expect("same account");
        assert_eq!(cumulative.input, u64::MAX);
        assert_eq!(cumulative.output, u64::MAX);
        assert_eq!(cumulative.reasoning, u64::MAX);
        assert_eq!(cumulative.cached, u64::MAX);
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
