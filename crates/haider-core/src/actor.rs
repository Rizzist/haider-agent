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
//!
//! General tool calls are surfaced (completed as `ToolStatus::Pending`) for a
//! later dispatcher. `request_input` is the intentional exception: the actor
//! owns its blocking menu round trip because only the actor may journal the
//! session's `MenuOpened`/`MenuAnswered` and run-state envelopes.

use crate::{StoreHandle, unix_time_ms};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{MenuAnswer, MenuCloseReason};
use haider_protocol::provider::{Block, FinishReason, StreamEvent};
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, Provider, ProviderError, ProviderErrorKind, TurnRequest};
use haider_tools::RequestInput;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

const DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN: usize = 32;
const DEFAULT_DEFERRED_COMMAND_CAPACITY: usize = 64;

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
    pub command_capacity: usize,
    pub broadcast_capacity: usize,
    /// Hard ceiling on provider requests made by one logical turn.
    pub max_provider_requests_per_turn: usize,
    /// Maximum number of submissions parked behind the active turn.
    pub deferred_command_capacity: usize,
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
            command_capacity: 8,
            broadcast_capacity: 128,
            max_provider_requests_per_turn: DEFAULT_MAX_PROVIDER_REQUESTS_PER_TURN,
            deferred_command_capacity: DEFAULT_DEFERRED_COMMAND_CAPACITY,
            started_at_ms: None,
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitTurn {
    pub text: String,
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
    async fn cancelled(&self) {
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
        request: SubmitTurn,
        accepted: oneshot::Sender<Result<TurnHandle, HaiderError>>,
    },
    AnswerMenu {
        answer: MenuAnswer,
        completed: oneshot::Sender<Result<(), HaiderError>>,
    },
}

enum MenuWake {
    Command(ActorCommand),
    Committed(RawEnvelope),
}

/// Single-session, single-writer run loop.
pub struct HarnessActor {
    config: HarnessConfig,
    provider: Arc<dyn Provider>,
    store: Arc<dyn StoreHandle>,
    commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RawEnvelope>,
    state: watch::Sender<Option<RunState>>,
    committed_menus: watch::Receiver<Option<RawEnvelope>>,
    next_run: u64,
    next_event: u64,
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
        let started_at_ms = config.started_at_ms.unwrap_or_else(unix_time_ms);
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
                store,
                commands,
                events,
                state,
                committed_menus: committed_menu_receiver,
                next_run: 0,
                next_event: 0,
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
            }
        }
    }

    /// Runs one turn to a terminal state. Every return path commits that
    /// terminal `RunState` (best effort) before reporting the outcome.
    async fn drive_turn(&mut self, submit: SubmitTurn, cancel: CancelToken) -> TurnOutcome {
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
        if let Err(error) = self.commit_state(&run_id, RunState::Thinking).await {
            return self.errored_state_outcome(&run_id, error).await;
        }

        let mut messages = vec![Message::user_text(submit.text)];
        let mut message: Option<TextAccumulator> = None;
        let mut reasoning: Option<TextAccumulator> = None;
        let mut tools: Vec<ToolAccumulator> = Vec::new();
        let mut provider_request_count = 0usize;

        'requests: loop {
            provider_request_count = provider_request_count.saturating_add(1);
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
            let provider_request = TurnRequest {
                messages: messages.clone(),
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system_prompt: None,
                tools: Vec::new(),
                attachments: Vec::new(),
            };
            let provider = Arc::clone(&self.provider);
            let mut opening = Box::pin(provider.stream_turn(provider_request));
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
            };
            if let Err(error) = self.commit_state(&run_id, RunState::Streaming).await {
                return self.errored_state_outcome(&run_id, error).await;
            }

            let mut assistant_blocks = Vec::new();
            let mut tool_results = Vec::new();
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
                    Ok(event) => event,
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
                        assistant_blocks.push(Block::Reasoning {
                            summary: text.clone(),
                        });
                        self.apply_text_delta(&run_id, &mut reasoning, text, true)
                            .await
                            .map(|()| None)
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
                                self.complete_tool(&run_id, &mut tools, &call_id, &cancel)
                                    .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                    StreamEvent::UsageUpdate(usage) => self
                        .commit_payload(&run_id, EventPayload::Usage(usage), prompt_omit_render())
                        .await
                        .map(|_| None)
                        .map_err(DriveError::Store),
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
                            .complete_all_tools(&run_id, &mut tools, ToolStatus::Pending)
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
                        if !tool_results.is_empty() {
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
        self.commit_tool_completed(run_id, &tools[index], ToolStatus::Pending)
            .await?;
        tools.remove(index);
        Ok(None)
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
        if let Err(commit_error) = self.commit_state(run_id, RunState::Errored).await {
            return errored_outcome(commit_error);
        }
        TurnOutcome {
            state: RunState::Errored,
            finish_reason: FinishReason::Error,
            error: Some(error),
        }
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

    /// Stamps identity/fencing fields, appends (the store assigns `seq` and
    /// `committed_at_ms`), then broadcasts the committed envelope.
    async fn commit_payload(
        &mut self,
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
        let mut envelopes = [EventEnvelope {
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
        }];
        self.store.append(&mut envelopes).await?;
        let [committed] = envelopes;
        // No live subscribers is fine — the store already has the envelope.
        let _ = self.events.send(committed.clone());
        Ok(committed)
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

    fn next_event_id(&mut self) -> EventId {
        // Globally unique across sessions, actors, and reopens: the store's
        // UNIQUE(event_id) constraint holds because the id embeds the session,
        // the actor's start instant, and a per-actor counter (merge-seam
        // contract with haider-store; review finding W1-B1 #3).
        self.next_event += 1;
        // worker_generation distinguishes same-ms actor restarts (fencing id).
        EventId::new(format!(
            "evt-{}-{}-{}-{}",
            self.config.session_id.as_str(),
            self.config.worker_generation,
            self.started_at_ms,
            self.next_event
        ))
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
    Store(HaiderError),
    Cancelled,
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

fn drive_error_to_haider(error: DriveError) -> HaiderError {
    match error {
        DriveError::Provider(error) => provider_error_to_haider(error),
        DriveError::Store(error) => error,
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
