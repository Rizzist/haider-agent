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
//!   ONLY here (adapters keep `RetryPolicy::Never`): at most three attempts
//!   per individual provider request, only retryable transport/rate-limit/
//!   overload errors, and never after that request emitted a stream event —
//!   which also fences effects, since a tool can only run after events.
//!   `wait_before_provider_retry` commits durable `Waiting` state around the
//!   backoff and cancellation wins every wait.
//!
//! General tool calls run through the injected [`ToolDispatcher`] (W3c);
//! with no dispatcher installed they are surfaced completed-as-
//! `ToolStatus::Pending`, the pre-W3c standalone behavior. `request_input`
//! is the intentional exception: the actor owns its blocking menu round trip
//! because only the actor may journal the session's
//! `MenuOpened`/`MenuAnswered` and run-state envelopes. Event ids come from
//! the [`EventIdGenerator`] namespace: supervisor-installed and shared with
//! the effect journal in the daemon, self-minted in standalone use.

use crate::{StoreHandle, unix_time_ms};
use async_trait::async_trait;
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::{AgentManifest, ChildReport, ChipState, ReportVerification};
use haider_protocol::credential::RotationEvent;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{
    AgentId, BranchId, CredentialAlias, DeviceId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuAnswer, MenuCloseReason};
use haider_protocol::provider::{
    AccountUsage, Block, FinishReason, PROVIDER_OPAQUE_EXTENSION_KIND, StreamEvent, Usage,
};
use haider_protocol::state::{RunState, WaitReason};
use haider_protocol::tool::BoundedResult;
use haider_provider::{
    Message, Provider, ProviderError, ProviderErrorKind, ResolvedAttachment, ToolDefinition,
    TurnRequest,
};
use haider_tools::RequestInput;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN: usize = 32;
const DEFAULT_DEFERRED_COMMAND_CAPACITY: usize = 64;
const MAX_PROVIDER_ATTEMPTS: usize = 3;
const RETRY_BASE_MS: u64 = 25;
const RETRY_CEILING_MS: u64 = 2_000;
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
    /// Deterministic daemon-owned policy bound to every request in this actor.
    pub system_prompt: Option<String>,
    /// General tools the paired dispatcher can execute.
    pub tools: Vec<ToolDefinition>,
    /// CAS-backed attachments resolved before crossing the provider boundary.
    pub attachments: Vec<ResolvedAttachment>,
    /// Account pinned by the turn-scoped provider resolver.
    pub usage_account: Option<CredentialAlias>,
    /// A factory-time alternate that must be committed before provider work.
    pub initial_rotation: Option<RotationEvent>,
    /// Logical-turn-wide one-hop allowance. This is distinct from provider
    /// attempt counters, which reset between tool-loop requests.
    pub rotation_budget_consumed: bool,
    /// Daemon-owned resolver consulted only at a pre-first-event boundary.
    pub provider_attempt_resolver: Option<Arc<dyn ProviderAttemptResolver>>,
    pub command_capacity: usize,
    pub broadcast_capacity: usize,
    /// Hard ceiling on provider requests made by one logical turn.
    pub max_provider_requests_per_turn: usize,
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
            system_prompt: None,
            tools: Vec::new(),
            attachments: Vec::new(),
            usage_account: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            provider_attempt_resolver: None,
            command_capacity: 8,
            broadcast_capacity: 128,
            max_provider_requests_per_turn: DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN,
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

    pub async fn submit_child_wait_turn(
        &self,
        request: SubmitChildWaitTurn,
    ) -> Result<TurnHandle, HaiderError> {
        self.submit(TurnSubmission::ChildWait(Box::new(request)))
            .await
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
    Stop {
        completed: oneshot::Sender<()>,
    },
}

enum TurnSubmission {
    Local(SubmitTurn),
    Committed(SubmitCommittedTurn),
    Checkpoint(Box<SubmitCheckpointTurn>),
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
    next_menu: u64,
    deferred_commands: VecDeque<ActorCommand>,
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
                next_menu: 0,
                deferred_commands: VecDeque::new(),
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
        let (run_id, mut messages, checkpoint, child_wait) = match submit {
            TurnSubmission::Local(submit) => {
                let run_id = self.next_run_id();
                if let Err(error) = self.commit_state(&run_id, RunState::Queued).await {
                    return self.errored_state_outcome(&run_id, error).await;
                }
                if let Err(error) = self
                    .commit_payload(
                        &run_id,
                        EventPayload::UserMessage {
                            text: submit.text.clone(),
                            attachments: Vec::new(),
                            mode: DeliveryMode::Steer,
                        },
                        prompt_verbatim_render(),
                    )
                    .await
                {
                    return self.errored_state_outcome(&run_id, error).await;
                }
                (run_id, vec![Message::user_text(submit.text)], None, None)
            }
            TurnSubmission::Committed(submit) => (submit.run_id, submit.messages, None, None),
            TurnSubmission::Checkpoint(submit) => {
                let submit = *submit;
                (
                    submit.run_id,
                    submit.messages,
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
                    Some(submit.checkpoint),
                )
            }
        };

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
        let mut provider_attempt = 0usize;
        let mut completed_usage: Option<Usage> = None;

        'requests: loop {
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
            let provider_request = TurnRequest {
                messages: messages.clone(),
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system_prompt: self.config.system_prompt.clone(),
                tools: self.config.tools.clone(),
                attachments: self.config.attachments.clone(),
            };
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
            let mut provider_event_seen = false;
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
                let Some(next) = next else {
                    let error =
                        provider_protocol_error("provider stream closed before a finish event");
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
                let event = match next {
                    Ok(event) => {
                        provider_event_seen = true;
                        event
                    }
                    Err(error) if !provider_event_seen => {
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
                    StreamEvent::RefusalDelta { .. } => {
                        // Refusal content has its own provider channel. The
                        // terminal Refusal outcome survives, but this content
                        // must never become assistant text or prompt history.
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
                    StreamEvent::UsageUpdate(mut usage) => {
                        if let Some(account) = &usage_account {
                            usage.account = Some(account.clone());
                            usage.accounts = vec![AccountUsage {
                                account: account.clone(),
                                input: usage.input,
                                output: usage.output,
                                reasoning: usage.reasoning,
                                cached: usage.cached,
                                source: usage.source,
                            }];
                        }
                        request_usage = Some(usage.clone());
                        match cumulative_usage(completed_usage.as_ref(), &usage) {
                            Ok(usage) => self
                                .commit_payload(
                                    &run_id,
                                    EventPayload::Usage(usage),
                                    prompt_omit_render(),
                                )
                                .await
                                .map(|_| None)
                                .map_err(DriveError::Store),
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
                            if let Some(usage) = request_usage.take() {
                                completed_usage =
                                    match cumulative_usage(completed_usage.as_ref(), &usage) {
                                        Ok(usage) => Some(usage),
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
                            }
                            provider_attempt = 0;
                            messages.append(&mut tool_results);
                            if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await
                            {
                                return self.errored_state_outcome(&run_id, error).await;
                            }
                            continue 'requests;
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
                    *provider = refreshed;
                    *account = Some(refreshed_account);
                    return Ok(());
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
        if provider_error_allows_retry(&error) && provider_attempt < MAX_PROVIDER_ATTEMPTS {
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

    /// Commits `Waiting { RateLimit | ProviderBackoff }`, sleeps, then
    /// commits `Thinking` — the R6 backoff between provider attempts.
    ///
    /// The delay honors `retry_after_ms` exactly through the one-minute
    /// respect cap. The two-second ceiling applies only to locally computed
    /// deterministic full jitter. Instructions beyond the respect cap are
    /// terminalized as retryable exhaustion instead of silently shortened.
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
            return Err(DriveError::Provider(ProviderError {
                kind: error.kind,
                message: format!(
                    "provider retry-after {}ms exceeds the {}ms respect cap",
                    error.retry_after_ms.unwrap_or_default(),
                    MAX_PROVIDER_RETRY_AFTER_MS
                ),
                retryable: true,
                retry_after_ms: error.retry_after_ms,
            }));
        }
        let reason = if error.kind == ProviderErrorKind::RateLimited {
            WaitReason::RateLimit
        } else {
            WaitReason::ProviderBackoff
        };
        self.commit_state(run_id, RunState::Waiting { reason })
            .await
            .map_err(DriveError::Store)?;
        let delay_ms = error.retry_after_ms.unwrap_or_else(|| {
            let exponent = u32::try_from(failed_attempt.saturating_sub(1)).unwrap_or(u32::MAX);
            let ceiling = RETRY_BASE_MS
                .saturating_mul(2_u64.saturating_pow(exponent))
                .min(RETRY_CEILING_MS);
            let mut hasher = blake3::Hasher::new();
            hasher.update(run_id.as_str().as_bytes());
            hasher.update(&failed_attempt.to_be_bytes());
            let bytes = hasher.finalize();
            let mut sample = [0_u8; 8];
            sample.copy_from_slice(&bytes.as_bytes()[..8]);
            u64::from_be_bytes(sample) % ceiling.saturating_add(1)
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(DriveError::Cancelled),
            () = tokio::time::sleep(std::time::Duration::from_millis(
                delay_ms,
            )) => {}
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
        if tools[index].name == "request_input" {
            return self
                .complete_request_input(run_id, tools, index, cancel)
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
            self.commit_tool_completed(run_id, &tools[index], ToolStatus::Completed)
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
                        RunState::InputRequired {
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
        self.commit_tool_completed(run_id, &tools[index], ToolStatus::Completed)
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
                preview: completion.report.summary.clone(),
                truncated: completion.truncated,
                artifact: None,
                cursor: None,
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
                self.commit_tool_completed(run_id, &tools[tool_index], ToolStatus::Completed)
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
        provider_error: ProviderError,
    ) -> TurnOutcome {
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
        self.errored_outcome_with_items(
            run_id,
            message,
            reasoning,
            tools,
            drive_error_to_haider(error),
        )
        .await
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
        self.commit_payload(run_id, EventPayload::Item(item), prompt_verbatim_render())
            .await
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
        payload: EventPayload,
        render: RenderTargets,
    ) -> Result<RawEnvelope, HaiderError> {
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
    error
}

fn provider_error_allows_retry(error: &ProviderError) -> bool {
    error.retryable
        && matches!(
            error.kind,
            ProviderErrorKind::Transport
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
        | ProviderErrorKind::InvalidRequest
        | ProviderErrorKind::Transport
        | ProviderErrorKind::MalformedFrame
        | ProviderErrorKind::InvalidUtf8
        | ProviderErrorKind::Internal => false,
    }
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
    })
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
        };
        let current = Usage {
            input: 1,
            output: 2,
            reasoning: 3,
            cached: 4,
            source: UsageSource::ProviderReported,
            account: None,
            accounts: Vec::new(),
        };
        let cumulative = cumulative_usage(Some(&completed), &current).expect("same account");
        assert_eq!(cumulative.input, u64::MAX);
        assert_eq!(cumulative.output, u64::MAX);
        assert_eq!(cumulative.reasoning, u64::MAX);
        assert_eq!(cumulative.cached, u64::MAX);
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
