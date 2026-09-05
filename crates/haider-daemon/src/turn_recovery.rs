//! CHARTER — durable interrupted-run reduction, performed before Ready (R5,
//! authoritative statement of the recovery rules).
//!
//! What lives here: the per-session journal reduction and the R5 verdict for
//! every prior-generation nonterminal run:
//!
//! - only accepted `Queued` (no provider request ever began) re-enqueues,
//!   with one aggregate `ActiveRun` per session gaining a recovered queue;
//! - durable `Cancelling` becomes `Cancelled` (items close `Cancelled`, an
//!   open menu closes `Cancelled`, no `RunFailed` — cancellation is not an
//!   error);
//! - a run parked at a durable tool-menu checkpoint (an open interactive or
//!   autonomous `request_input`, or broker-approved mutating tool without its
//!   `ToolResult`) is reconstructed with its exact session policy; blocking
//!   menus remain waiters while autonomous nonblocking menus settle without
//!   repeating the provider request or any dispatched effect;
//! - an autonomous `plan` interrupted after its non-blocking `MenuOpened` is
//!   reconstructed from `Streaming` (or legacy `InputRequired`) so the actor
//!   immediately journals acceptance and continues without a waiter;
//! - a run durably waiting on network reachability is reconstructed with its
//!   response-epoch text, reasoning, refusal, structured prefix, open tool
//!   accumulators, completed tool results, and original accepted-run deadline;
//!   exact provider replay is suppressed and completed effects are not
//!   dispatched again;
//! - an active delegated child is left nonterminal for its recovered parent's
//!   durable child-wait coordinator. W6c re-arms the progress deadline from
//!   committed envelope time, delivers at most one steer, and then uses the
//!   ordinary durable cancel path; provider/tool work is never redispatched;
//! - a first provider request admitted before the crash, but with no durable
//!   response, parks as an outcome-unknown network effect and recovery menu
//!   unless an active workflow run has an explicit durable budget. That
//!   composite admission is a recoverable request boundary: startup restores
//!   its logical spend and ordinal coordinates and re-enters the worker
//!   without counting the response-free attempt as completed spend;
//! - every other nonterminal run terminalizes: open items close `Failed`, an
//!   open menu closes `RecoveryInterrupted`, one sanitized retryable
//!   `RunFailed` precedes `Errored`, and the session settles
//!   `Idle { interrupted: true }` — all in ONE transactional batch per run,
//!   so a rerun sees the whole terminal batch or none.
//!
//! What may NOT live here: starting provider work (the `WorkerManager`
//! receives the recovered work AFTER this pass; recovery never opens a
//! provider stream or dispatches an effect), hub or wire concerns (this runs
//! on the raw pre-hub `SqliteStoreHandle` — the one sanctioned direct-store
//! writer besides effect reconciliation), and current-generation runs (live
//! workers own those; the generation fence skips them).

use crate::delegation::{DelegationMirrorHandoff, DelegationMirrorHandoffPhase};

use haider_core::{
    AcceptedRunRetry, AcceptedTurn, ChildWaitCheckpoint, DeferredTicket, DeferredToolCheckpoint,
    PartialStreamCheckpoint, ROUTE_REPLAY_ATTEMPT_EXTENSION_KIND,
    ROUTE_REPLAY_EVENT_EXTENSION_KIND, RequestInputCheckpoint, RouteWaitCheckpoint,
    RouteWaitCompletedToolCheckpoint, RouteWaitTextCheckpoint, RouteWaitToolCheckpoint,
    SessionProjectionCheckpoint, SqliteStoreHandle, StoreHandle, TurnAdmissionDisposition,
};
use haider_protocol::EventPayload;
use haider_protocol::cache::{CacheRequestAttemptV1, ProviderRequestAttemptV1};
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RawPayload, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::graph::{GraphFinalizationDeferred, GraphPhase};
use haider_protocol::headless::{HeadlessRunEventPayload, RunBudgetExhaustedV1, RunBudgetV1};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EffectId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason, MenuKind, effect_recovery_menu};
use haider_protocol::provider::{PROVIDER_OPAQUE_EXTENSION_KIND, StreamEvent};
use haider_protocol::reply::{ReplyArenaWriter, ReplyText};
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::state::{RunState, SessionState, WaitReason};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const PAGE_SIZE: usize = 512;
const PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const RUN_STATE_PAYLOAD_KINDS: &[&str] = &["run_state"];
pub(crate) const STARTUP_HYDRATION_PAYLOAD_KINDS: &[&str] = &[
    "agent_report",
    "effect",
    "graph_finalization_deferred",
    "headless_run_configured",
    "hook_run_trust",
    "item",
    "item_tool_call",
    "menu_answered",
    "menu_closed",
    "menu_opened",
    "node_committed",
    "run_budget_exhausted",
    "run_failed",
    "run_retried",
    "run_state",
    "tool_result",
    "usage",
    "user_message",
];
const CHECKPOINT_PROJECTION: &str = "startup_turn_recovery";
const CHECKPOINT_TIMELINE: &str = "session";
const CHECKPOINT_SHAPE_VERSION: u32 = 1;
// v6 adds the provider-response coordinate used to distinguish an admitted
// request with no response from an interrupted response stream. Earlier
// cursors cannot prove that retry boundary. v7 retains the durable run budget
// needed to distinguish a recoverable active-workflow admission from ordinary
// ambiguous provider delivery. Reject either older cursor and perform one
// complete ordered journal reduction.
// v8 separates the first logical model boundary from the physical request
// ordinal, because a turn-owned warmup or cache-resource request may consume
// ordinal 1 before the first model request.
const CHECKPOINT_REDUCER_VERSION: &str = "startup-turn-recovery-v8";

pub(crate) enum RecoveredWork {
    Queued(RecoveredQueued),
    Retry(RecoveredRetry),
    Checkpoint(Box<RecoveredCheckpoint>),
    PartialStream(Box<RecoveredPartialStream>),
    RouteWait(Box<RecoveredRouteWait>),
    ChildWait(Box<RecoveredChildWait>),
    AdmissionRetry(Box<RecoveredAdmissionRetry>),
    WorkflowContinuation(Box<RecoveredWorkflowContinuation>),
    DelegationMirror(Box<RecoveredDelegationMirror>),
}

pub(crate) struct RecoveredQueued {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredRetry {
    pub(crate) accepted: AcceptedRunRetry,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredAdmissionRetry {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) provider_requests_consumed: usize,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredWorkflowContinuation {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) provider_requests_consumed: usize,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredDelegationMirror {
    pub(crate) record: haider_core::DelegationRecord,
    pub(crate) handoff: DelegationMirrorHandoff,
}

pub(crate) struct RecoveredRouteWait {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: RouteWaitCheckpoint,
    pub(crate) provider_requests_consumed: usize,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredPartialStream {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: PartialStreamCheckpoint,
    pub(crate) committed_answer: Option<RawEnvelope>,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredChildWait {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: ChildWaitCheckpoint,
    pub(crate) provider_request_ordinal: u64,
}

pub(crate) struct RecoveredCheckpoint {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: RequestInputCheckpoint,
    pub(crate) committed_answer: Option<RawEnvelope>,
    pub(crate) provider_request_ordinal: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct RunReduction {
    branch_id: Option<BranchId>,
    branch_observed: bool,
    branch_mismatch: bool,
    state: Option<(RunState, u64)>,
    state_generation: u64,
    user_seq: Option<u64>,
    retry_source: Option<(RunId, RunId, u64, u64)>,
    open_items: HashMap<ItemId, OpenItem>,
    incomplete_items: HashMap<ItemId, ReplyText>,
    menu: Option<OpenMenu>,
    menu_answers: HashMap<MenuId, RawEnvelope>,
    tool_results: HashSet<String>,
    #[serde(default)]
    tool_result_values: HashMap<String, haider_protocol::tool::BoundedResult>,
    tool_calls: HashMap<String, RecoveredToolCall>,
    agent_reports: HashSet<AgentId>,
    child_results: HashSet<AgentId>,
    #[serde(default)]
    headless_configured: bool,
    #[serde(default)]
    headless_budget: RunBudgetV1,
    #[serde(default)]
    budget_exhausted: Option<RunBudgetExhaustedV1>,
    #[serde(default)]
    workflow_deferral: Option<(GraphFinalizationDeferred, u64)>,
    #[serde(default)]
    latest_provider_request_attempt: Option<(u64, u64)>,
    /// Number of durable model-boundary attempts. Auxiliary physical HTTP
    /// calls do not make an admitted model request a continuation.
    #[serde(default)]
    provider_model_request_attempt_count: u64,
    /// Highest physical request identity across model and auxiliary
    /// turn-owned HTTP calls. The sequence-bearing field above remains the
    /// model-response recovery boundary.
    #[serde(default)]
    provider_request_ordinal_max: u64,
    #[serde(default)]
    provider_request_attempt_corrupt: bool,
    #[serde(default)]
    provider_request_turn_ordinal: Option<u64>,
    #[serde(default)]
    latest_provider_response_seq: Option<u64>,
    #[serde(default)]
    delegation_mirror_handoffs: HashMap<String, DelegationMirrorHandoff>,
    route_replay_epoch: u64,
    #[serde(default)]
    route_replay_events: Vec<StreamEvent>,
}

#[derive(Serialize, Deserialize)]
struct RecoveredToolCall {
    item_id: ItemId,
    name: String,
    args: String,
    started_seq: u64,
    completed: bool,
}

struct OpenItem {
    item: TurnItem,
    started_seq: u64,
    text: ReplyArenaWriter,
    args: String,
}

impl Serialize for OpenItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Encoded<'a> {
            item: &'a TurnItem,
            started_seq: u64,
            text: ReplyText,
            args: &'a str,
        }
        Encoded {
            item: &self.item,
            started_seq: self.started_seq,
            text: self.text.snapshot(),
            args: &self.args,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OpenItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Encoded {
            item: TurnItem,
            started_seq: u64,
            text: String,
            args: String,
        }
        let encoded = Encoded::deserialize(deserializer)?;
        let mut text = ReplyArenaWriter::new();
        let _ = text.append(encoded.text);
        Ok(Self {
            item: encoded.item,
            started_seq: encoded.started_seq,
            text,
            args: encoded.args,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct OpenMenu {
    menu: Menu,
    request_seq: u64,
    opening_generation: u64,
}

pub(crate) struct StartupTurnRecovery {
    pub(crate) work: Vec<RecoveredWork>,
    pub(crate) touched_sessions: Vec<SessionId>,
}

impl StartupTurnRecovery {
    pub(crate) fn schema_zero() -> Self {
        Self {
            work: Vec::new(),
            touched_sessions: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct DurableTurnRecoveryCheckpoint {
    shape_version: u32,
    reducer_version: String,
    through_seq: u64,
    reductions: HashMap<RunId, RunReduction>,
}

#[derive(Serialize)]
struct DurableTurnRecoveryCheckpointRef<'a> {
    shape_version: u32,
    reducer_version: &'static str,
    through_seq: u64,
    reductions: &'a HashMap<RunId, RunReduction>,
}

/// Neutral consumer for journal pages already decoded by startup recovery.
///
/// Runtime uses this seam to hydrate native sidecars and hook reducer state
/// from the same per-session scan. The visitor may request an earlier cursor
/// than the turn reducer's checkpoint, but it never owns recovery decisions
/// or writes run state.
#[async_trait::async_trait]
pub(crate) trait StartupJournalVisitor: Send {
    async fn start_session(&mut self, session_id: &SessionId) -> Result<u64, HaiderError>;

    async fn visit_page(
        &mut self,
        session_id: &SessionId,
        page: &[RawEnvelope],
    ) -> Result<(), HaiderError>;

    async fn finish_session(
        &mut self,
        store: &SqliteStoreHandle,
        session_id: &SessionId,
    ) -> Result<(), HaiderError>;
}

#[cfg(test)]
#[allow(clippy::expect_used)]
struct NoopStartupJournalVisitor;

#[cfg(test)]
#[allow(clippy::expect_used)]
#[async_trait::async_trait]
impl StartupJournalVisitor for NoopStartupJournalVisitor {
    async fn start_session(&mut self, _session_id: &SessionId) -> Result<u64, HaiderError> {
        Ok(u64::MAX)
    }

    async fn visit_page(
        &mut self,
        _session_id: &SessionId,
        _page: &[RawEnvelope],
    ) -> Result<(), HaiderError> {
        Ok(())
    }

    async fn finish_session(
        &mut self,
        _store: &SqliteStoreHandle,
        _session_id: &SessionId,
    ) -> Result<(), HaiderError> {
        Ok(())
    }
}

pub(crate) async fn recover_interrupted_turns_report_with_visitor(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    visitor: &mut dyn StartupJournalVisitor,
) -> Result<StartupTurnRecovery, HaiderError> {
    let mut recovered = Vec::new();
    let mut touched_sessions = Vec::new();
    for session_id in store.session_ids().await? {
        let mut touched = false;
        let runnable_metadata = store.session_metadata(&session_id).await?.is_some();
        let (mut reductions, turn_cursor) = load_recovery_checkpoint(store, &session_id).await?;
        let visitor_cursor = visitor.start_session(&session_id).await?;
        let mut cursor = turn_cursor.min(visitor_cursor);
        let mut boundary = None;
        loop {
            let page = store
                .read_reducer_page_with_boundary(
                    &session_id,
                    cursor,
                    PAGE_SIZE,
                    PAGE_BYTES,
                    STARTUP_HYDRATION_PAYLOAD_KINDS,
                )
                .await?;
            if page.envelopes.is_empty() {
                if let Some((through_seq, boundary_event_id)) = page.observed_head
                    && through_seq > turn_cursor
                {
                    boundary = Some((through_seq, boundary_event_id));
                }
                break;
            }
            cursor = page
                .envelopes
                .last()
                .map_or(cursor, |envelope| envelope.seq);
            for envelope in page
                .envelopes
                .iter()
                .filter(|envelope| envelope.seq > turn_cursor)
            {
                reduce(&mut reductions, envelope);
            }
            visitor.visit_page(&session_id, &page.envelopes).await?;
            reductions.retain(|_, reduction| {
                reduction.branch_mismatch
                    || !reduction.delegation_mirror_handoffs.is_empty()
                    || !reduction
                        .state
                        .as_ref()
                        .is_some_and(|(state, _)| state.is_terminal())
            });
            if let Some(last) = page
                .envelopes
                .last()
                .filter(|envelope| envelope.seq > turn_cursor)
            {
                boundary = Some((last.seq, last.event_id.clone()));
            }
        }
        if let Some((through_seq, boundary_event_id)) = boundary {
            put_recovery_checkpoint(
                store,
                &session_id,
                through_seq,
                boundary_event_id,
                &reductions,
            )
            .await?;
        }
        let mut runs = reductions.into_iter().collect::<Vec<_>>();
        runs.sort_by_key(|(_, reduction)| {
            reduction
                .user_seq
                .or_else(|| reduction.retry_source.as_ref().map(|(_, _, seq, _)| *seq))
                .unwrap_or(u64::MAX)
        });
        let mut activated_recovered_queue = false;
        for (run_id, reduction) in runs {
            if reduction.branch_mismatch {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("run {run_id} crosses branch scopes"),
                    false,
                ));
            }
            if reduction.provider_request_attempt_corrupt {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("run {run_id} has a malformed provider request-attempt marker"),
                    false,
                ));
            }
            let Some((state, _)) = reduction.state.clone() else {
                continue;
            };
            // Composite recovery invariant (deleg + wfcont + resume + maxcost),
            // in this order:
            // 1. validate and enqueue every durable cancellation-mirror
            //    obligation, including obligations on terminal/current runs;
            // 2. terminalize a stale nonterminal child with a pending mirror
            //    as Cancelled and stop considering that run;
            // 3. only with no mirror may workflow continuation restore its
            //    logical request count and physical attempt ordinal;
            // 4. reconstruct partial/completed response streams and a route
            //    retry from the same logical coordinates before local-child
            //    waits and generic recovery. Re-admission rebuilds maxcost's
            //    one active projection only at the next physical send.
            // Cancellation ownership must dominate resumable provider work or
            // a crash can resurrect a delegated workflow after its parent has
            // durably requested cancellation.
            let mut mirror_handoffs = reduction
                .delegation_mirror_handoffs
                .values()
                .cloned()
                .collect::<Vec<_>>();
            mirror_handoffs.sort_by(|left, right| left.handoff_id.cmp(&right.handoff_id));
            for handoff in &mirror_handoffs {
                if handoff.child_session_id != session_id || handoff.child_run_id != run_id {
                    return Err(HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!(
                            "delegation mirror handoff {} crosses child run coordinates",
                            handoff.handoff_id
                        ),
                        false,
                    ));
                }
                let record = store
                    .delegation(handoff.agent.clone())
                    .await?
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!(
                                "delegation mirror handoff {} has no durable delegation",
                                handoff.handoff_id
                            ),
                            false,
                        )
                    })?;
                recovered.push(RecoveredWork::DelegationMirror(Box::new(
                    RecoveredDelegationMirror {
                        record,
                        handoff: handoff.clone(),
                    },
                )));
            }
            if state.is_terminal() {
                continue;
            }
            if reduction.state_generation == store.worker_generation() {
                continue;
            }
            if !mirror_handoffs.is_empty() {
                // The pending child-journal fact precedes the live cancellation
                // wake. A daemon crash in that window therefore recovers as a
                // cancellation, never as generic interruption; the queued
                // mirror remains until its completion fact is journaled.
                terminalize_interrupted(
                    store,
                    device_id,
                    &session_id,
                    &run_id,
                    reduction.branch_id.clone(),
                    reduction,
                    true,
                )
                .await?;
                touched = true;
                continue;
            }
            // Acceptance now requires typed metadata, but legacy/CLI journals
            // can predate that guarantee. No prior-generation runnable shape
            // (Queued or checkpointed) may become startup poison.
            if !runnable_metadata {
                terminalize_interrupted(
                    store,
                    device_id,
                    &session_id,
                    &run_id,
                    reduction.branch_id.clone(),
                    reduction,
                    matches!(state, RunState::Cancelling),
                )
                .await?;
                touched = true;
                continue;
            }
            let turn_ordinal = store
                .turn_ordinal(session_id.clone(), run_id.clone())
                .await?
                .ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("run {run_id} has no durable turn ordinal"),
                        false,
                    )
                })?;
            if reduction
                .provider_request_turn_ordinal
                .is_some_and(|journal_ordinal| journal_ordinal != turn_ordinal)
            {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "run {run_id} request-attempt turn ordinal disagrees with its durable run"
                    ),
                    false,
                ));
            }
            if state == RunState::Queued {
                let provider_request_ordinal = recovered_provider_request_ordinal(&reduction);
                if let Some(accepted_seq) = reduction.user_seq {
                    if !activated_recovered_queue {
                        append_recovered_active(store, device_id, &session_id, &run_id).await?;
                        touched = true;
                        activated_recovered_queue = true;
                    }
                    recovered.push(RecoveredWork::Queued(RecoveredQueued {
                        accepted: recovered_acceptance(
                            &session_id,
                            &run_id,
                            turn_ordinal,
                            accepted_seq,
                            store.worker_generation(),
                            reduction.branch_id.clone(),
                        ),
                        provider_request_ordinal,
                    }));
                } else if let Some((failed_run_id, prompt_run_id, user_seq, accepted_seq)) =
                    reduction.retry_source
                {
                    if !activated_recovered_queue {
                        append_recovered_active(store, device_id, &session_id, &run_id).await?;
                        touched = true;
                        activated_recovered_queue = true;
                    }
                    recovered.push(RecoveredWork::Retry(RecoveredRetry {
                        accepted: AcceptedRunRetry {
                            session_id: session_id.clone(),
                            run_id,
                            turn_ordinal,
                            failed_run_id,
                            prompt_run_id,
                            user_seq,
                            accepted_seq,
                            worker_generation: store.worker_generation(),
                            backoff_event_id: None,
                        },
                        provider_request_ordinal,
                    }));
                }
                continue;
            }
            if let Some(retry) = pending_admission_retry(
                store,
                &session_id,
                &run_id,
                turn_ordinal,
                &state,
                &reduction,
            )? {
                let graph_phase = store
                    .graph_status(&session_id)
                    .await?
                    .map(|status| status.phase);
                if budgeted_workflow_admission_is_recoverable(&reduction, graph_phase) {
                    recovered.push(RecoveredWork::AdmissionRetry(Box::new(retry)));
                } else {
                    park_ambiguous_admission_retry(store, device_id, &retry).await?;
                    touched = true;
                }
                continue;
            }
            if let Some(continuation) = pending_workflow_continuation(
                store,
                &session_id,
                &run_id,
                turn_ordinal,
                &state,
                &reduction,
            )
            .await?
            {
                recovered.push(RecoveredWork::WorkflowContinuation(Box::new(continuation)));
                continue;
            }
            if let Some(checkpoint) = pending_checkpoint(&reduction)
                && checkpoint_state_matches(&state, &checkpoint)
            {
                let committed_answer = reduction.menu_answers.get(&checkpoint.menu.id).cloned();
                let accepted_seq = reduction.user_seq.ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("checkpoint run {run_id} has no user message"),
                        false,
                    )
                })?;
                recovered.push(RecoveredWork::Checkpoint(Box::new(RecoveredCheckpoint {
                    accepted: recovered_acceptance(
                        &session_id,
                        &run_id,
                        turn_ordinal,
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    ),
                    checkpoint,
                    committed_answer,
                    provider_request_ordinal: recovered_provider_request_ordinal(&reduction),
                })));
                continue;
            }
            if let Some(checkpoint) = pending_partial_stream_checkpoint(&reduction)
                && partial_stream_checkpoint_state_matches(&state, &checkpoint)
            {
                let committed_answer = reduction.menu_answers.get(&checkpoint.menu.id).cloned();
                let accepted_seq = reduction.user_seq.ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("partial-stream checkpoint run {run_id} has no user message"),
                        false,
                    )
                })?;
                recovered.push(RecoveredWork::PartialStream(Box::new(
                    RecoveredPartialStream {
                        accepted: recovered_acceptance(
                            &session_id,
                            &run_id,
                            turn_ordinal,
                            accepted_seq,
                            store.worker_generation(),
                            reduction.branch_id.clone(),
                        ),
                        checkpoint,
                        committed_answer,
                        provider_request_ordinal: recovered_provider_request_ordinal(&reduction),
                    },
                )));
                continue;
            }
            if matches!(
                state,
                RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                }
            ) && let Some(checkpoint) = pending_route_wait_checkpoint(&reduction)
            {
                let accepted_seq = reduction.user_seq.ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("route-wait run {run_id} has no user message"),
                        false,
                    )
                })?;
                let provider_requests_consumed =
                    usize::try_from(checkpoint.response_epoch.saturating_add(1)).map_err(|_| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!(
                                "route-wait response epoch {} does not fit this platform",
                                checkpoint.response_epoch
                            ),
                            false,
                        )
                    })?;
                let provider_request_ordinal = recovered_provider_request_ordinal(&reduction);
                let provider_request_ordinal = (provider_request_ordinal != 0)
                    .then_some(provider_request_ordinal)
                    .ok_or_else(|| {
                        HaiderError::new(
                            ErrorCode::StoreCorrupt,
                            format!("route-wait run {run_id} has no provider request attempt"),
                            false,
                        )
                    })?;
                recovered.push(RecoveredWork::RouteWait(Box::new(RecoveredRouteWait {
                    accepted: recovered_acceptance(
                        &session_id,
                        &run_id,
                        turn_ordinal,
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    ),
                    checkpoint,
                    provider_requests_consumed,
                    provider_request_ordinal,
                })));
                continue;
            }
            if matches!(
                state,
                RunState::Waiting {
                    reason: WaitReason::LocalChild
                }
            ) && let Some(checkpoint) =
                pending_child_wait(store, &session_id, &run_id, &reduction).await?
            {
                let accepted_seq = reduction.user_seq.ok_or_else(|| {
                    HaiderError::new(
                        ErrorCode::StoreCorrupt,
                        format!("child-wait run {run_id} has no user message"),
                        false,
                    )
                })?;
                recovered.push(RecoveredWork::ChildWait(Box::new(RecoveredChildWait {
                    accepted: recovered_acceptance(
                        &session_id,
                        &run_id,
                        turn_ordinal,
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    ),
                    checkpoint,
                    provider_request_ordinal: recovered_provider_request_ordinal(&reduction),
                })));
                continue;
            }
            let supervised_by_waiting_parent = if state == RunState::Cancelling {
                false
            } else if let Some(delegation) = store
                .delegation_for_child_session(session_id.clone())
                .await?
                .filter(|delegation| delegation.child_run_id == run_id)
            {
                matches!(
                    latest_run_state(
                        store,
                        &delegation.parent_session_id,
                        &delegation.parent_run_id,
                        delegation.parent_branch_id.as_ref(),
                    )
                    .await?,
                    Some(RunState::Waiting {
                        reason: WaitReason::LocalChild
                    })
                )
            } else {
                false
            };
            if supervised_by_waiting_parent {
                // The recovered parent ChildWait owns this child from here.
                // Reissuing its interrupted provider/tool work would be
                // unsafe; terminalizing it here would defeat durable stall
                // supervision. It therefore remains parked until progress,
                // nudge, or cancellation settles the delegation.
                continue;
            }
            terminalize_interrupted(
                store,
                device_id,
                &session_id,
                &run_id,
                reduction.branch_id.clone(),
                reduction,
                matches!(state, RunState::Cancelling),
            )
            .await?;
            touched = true;
        }
        if touched {
            touched_sessions.push(session_id.clone());
        }
        visitor.finish_session(store, &session_id).await?;
    }
    Ok(StartupTurnRecovery {
        work: recovered,
        touched_sessions,
    })
}

async fn load_recovery_checkpoint(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
) -> Result<(HashMap<RunId, RunReduction>, u64), HaiderError> {
    let checkpoint = match store
        .projection_checkpoint(
            session_id,
            CHECKPOINT_PROJECTION.to_owned(),
            CHECKPOINT_TIMELINE.to_owned(),
        )
        .await
    {
        Ok(checkpoint) => checkpoint,
        Err(error) if error.code == ErrorCode::StoreCorrupt => return Ok(Default::default()),
        Err(error) => return Err(error),
    };
    let Some(checkpoint) = checkpoint else {
        return Ok(Default::default());
    };
    let decoded = match rmp_serde::from_slice::<DurableTurnRecoveryCheckpoint>(&checkpoint.payload)
    {
        Ok(decoded) => decoded,
        Err(_) => return Ok(Default::default()),
    };
    if decoded.shape_version != CHECKPOINT_SHAPE_VERSION
        || decoded.reducer_version != CHECKPOINT_REDUCER_VERSION
        || decoded.through_seq != checkpoint.through_seq
    {
        return Ok(Default::default());
    }
    Ok((decoded.reductions, decoded.through_seq))
}

async fn put_recovery_checkpoint(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    through_seq: u64,
    boundary_event_id: EventId,
    reductions: &HashMap<RunId, RunReduction>,
) -> Result<(), HaiderError> {
    let payload = rmp_serde::to_vec_named(&DurableTurnRecoveryCheckpointRef {
        shape_version: CHECKPOINT_SHAPE_VERSION,
        reducer_version: CHECKPOINT_REDUCER_VERSION,
        through_seq,
        reductions,
    })
    .map_err(|error| {
        HaiderError::new(
            ErrorCode::Internal,
            format!("cannot encode startup turn-recovery checkpoint: {error}"),
            false,
        )
    })?;
    store
        .put_projection_checkpoint(SessionProjectionCheckpoint {
            session_id: session_id.clone(),
            projection: CHECKPOINT_PROJECTION.to_owned(),
            timeline_key: CHECKPOINT_TIMELINE.to_owned(),
            through_seq,
            boundary_event_id,
            payload,
        })
        .await
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) async fn recover_interrupted_turns(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
) -> Result<Vec<RecoveredWork>, HaiderError> {
    Ok(recover_interrupted_turns_report_with_visitor(
        store,
        device_id,
        &mut NoopStartupJournalVisitor,
    )
    .await?
    .work)
}

fn pending_admission_retry(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    turn_ordinal: u64,
    state: &RunState,
    reduction: &RunReduction,
) -> Result<Option<RecoveredAdmissionRetry>, HaiderError> {
    if !admitted_first_request_has_no_response(state, reduction) {
        return Ok(None);
    }
    let accepted_seq = reduction.user_seq.ok_or_else(|| {
        HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("admission-retry run {run_id} has no user message"),
            false,
        )
    })?;
    let provider_request_ordinal = recovered_provider_request_ordinal(reduction);
    Ok(Some(RecoveredAdmissionRetry {
        accepted: recovered_acceptance(
            session_id,
            run_id,
            turn_ordinal,
            accepted_seq,
            store.worker_generation(),
            reduction.branch_id.clone(),
        ),
        // The durable marker consumed one physical identity. A recovery
        // reissue is a new attempt and must therefore allocate N+1.
        provider_requests_consumed: 0,
        provider_request_ordinal,
    }))
}

async fn park_ambiguous_admission_retry(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    retry: &RecoveredAdmissionRetry,
) -> Result<(), HaiderError> {
    let attempt = retry.provider_request_ordinal.saturating_add(1);
    let run_id = &retry.accepted.run_id;
    let session_id = &retry.accepted.session_id;
    let effect_id = EffectId::new(format!("provider-admission-{run_id}-{attempt}"));
    let menu_id = MenuId::new(format!("provider-admission-recovery-{run_id}-{attempt}"));
    let summary = format!("provider request attempt {attempt} admitted before daemon restart");
    let args_digest = format!(
        "sha256:{:x}",
        Sha256::digest(format!("{session_id}\0{run_id}\0{attempt}").as_bytes())
    );
    let payloads = vec![
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect_id.clone(),
            class: EffectClass::Network {
                host: "provider-request".into(),
            },
            summary: summary.clone(),
            args_digest,
            workspace_revision: None,
        })),
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect_id.clone(),
            verdict: AuthorizationVerdict::Allow,
        }),
        EventPayload::Effect(EffectPhase::Dispatched {
            effect: effect_id.clone(),
        }),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: effect_id.clone(),
            outcome: EffectOutcome::Unknown,
            freshness: None,
            workspace_mutation: None,
        }),
        EventPayload::MenuOpened(effect_recovery_menu(menu_id, effect_id, summary)),
        EventPayload::RunState(RunState::EffectOutcomeUnknown),
    ];
    let mut envelopes = recovery_envelopes(
        store.worker_generation(),
        device_id,
        session_id,
        run_id,
        retry.accepted.branch_id.as_ref(),
        payloads,
    )?;
    store.append(&mut envelopes).await?;
    Ok(())
}

fn admitted_first_request_has_no_response(state: &RunState, reduction: &RunReduction) -> bool {
    let Some((attempt_seq, _ordinal)) = reduction.latest_provider_request_attempt else {
        return false;
    };
    let state_matches_attempt = match reduction.state.as_ref() {
        Some((RunState::Thinking, state_seq)) if *state == RunState::Thinking => {
            *state_seq < attempt_seq
        }
        Some((RunState::Streaming, state_seq)) if *state == RunState::Streaming => {
            attempt_seq < *state_seq
        }
        _ => false,
    };
    reduction.provider_model_request_attempt_count == 1
        && reduction.workflow_deferral.is_none()
        && state_matches_attempt
        && reduction.headless_configured
        && reduction.budget_exhausted.is_none()
        && reduction.open_items.is_empty()
        && reduction.incomplete_items.is_empty()
        && reduction.menu.is_none()
        && reduction.delegation_mirror_handoffs.is_empty()
        && reduction.route_replay_events.is_empty()
        && reduction
            .latest_provider_response_seq
            .is_none_or(|response_seq| response_seq < attempt_seq)
}

fn budgeted_workflow_admission_is_recoverable(
    reduction: &RunReduction,
    graph_phase: Option<GraphPhase>,
) -> bool {
    reduction.headless_budget.has_shared_limits() && graph_phase == Some(GraphPhase::Active)
}

async fn pending_workflow_continuation(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    turn_ordinal: u64,
    state: &RunState,
    reduction: &RunReduction,
) -> Result<Option<RecoveredWorkflowContinuation>, HaiderError> {
    if !workflow_continuation_shape_is_eligible(state, reduction) {
        return Ok(None);
    }
    let Some((deferred, deferred_seq)) = reduction.workflow_deferral.as_ref() else {
        return Ok(None);
    };
    // A request-attempt marker is committed before provider open. Once one
    // exists after the deferral, provider delivery is ambiguous and generic
    // interrupted-stream recovery must remain fail-closed.
    if !provider_request_precedes_deferral(
        reduction
            .latest_provider_request_attempt
            .map(|(seq, _)| seq),
        *deferred_seq,
    ) || &deferred.run_id != run_id
    {
        return Ok(None);
    }
    let Some(status) = store.graph_status(session_id).await? else {
        return Ok(None);
    };
    if status.phase != GraphPhase::Active
        || status.graph_id != deferred.graph_id
        || haider_store::graph_finalization_state_digest(&status)? != deferred.state_digest
    {
        return Ok(None);
    }
    let accepted_seq = reduction.user_seq.ok_or_else(|| {
        HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("workflow continuation run {run_id} has no user message"),
            false,
        )
    })?;
    let provider_requests_consumed =
        usize::try_from(deferred.provider_requests_consumed).map_err(|_| {
            HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "workflow continuation request count {} does not fit this platform",
                    deferred.provider_requests_consumed
                ),
                false,
            )
        })?;
    Ok(Some(RecoveredWorkflowContinuation {
        accepted: recovered_acceptance(
            session_id,
            run_id,
            turn_ordinal,
            accepted_seq,
            store.worker_generation(),
            reduction.branch_id.clone(),
        ),
        provider_requests_consumed,
        provider_request_ordinal: recovered_provider_request_ordinal(reduction),
    }))
}

fn workflow_continuation_shape_is_eligible(state: &RunState, reduction: &RunReduction) -> bool {
    *state == RunState::Streaming
        && reduction.headless_configured
        && reduction.budget_exhausted.is_none()
        && reduction.open_items.is_empty()
        && reduction.incomplete_items.is_empty()
        && reduction.menu.is_none()
        // Defense in depth for the recovery-order invariant: even if runnable
        // classification is later rearranged, durable cancellation intent can
        // never be projected as resumable provider work.
        && reduction.delegation_mirror_handoffs.is_empty()
}

fn provider_request_precedes_deferral(
    latest_provider_request_attempt_seq: Option<u64>,
    deferred_seq: u64,
) -> bool {
    latest_provider_request_attempt_seq.is_some_and(|attempt_seq| attempt_seq < deferred_seq)
}

fn checkpoint_state_matches(state: &RunState, checkpoint: &RequestInputCheckpoint) -> bool {
    match state {
        // A present-and-proceed plan can crash after MenuOpened but before its
        // automatic answer/result. It deliberately never writes
        // InputRequired, so the open non-blocking plan document plus its open
        // tool item is the durable checkpoint while the run remains Streaming.
        RunState::Streaming
            if !checkpoint.menu.blocking
                && (checkpoint.tool_name == "request_input"
                    || (checkpoint.tool_name == "plan"
                        && checkpoint.menu.origin == haider_tools::PLAN_ORIGIN)) =>
        {
            true
        }
        // Dual-read migration: old broker approvals and actor-owned input
        // checkpoints both used InputRequired. This also resumes legacy plan
        // menus written before plans became autonomous.
        RunState::InputRequired { menu } => *menu == checkpoint.menu.id,
        // New permission vocabulary is valid only for an actual committed
        // permission menu; accepting another kind would hide corruption.
        RunState::PermissionRequired { menu } => {
            *menu == checkpoint.menu.id
                && matches!(checkpoint.menu.kind, MenuKind::Permission { .. })
        }
        _ => false,
    }
}

fn partial_stream_checkpoint_state_matches(
    state: &RunState,
    checkpoint: &PartialStreamCheckpoint,
) -> bool {
    matches!(
        state,
        RunState::InputRequired { menu } if *menu == checkpoint.menu.id
    ) || matches!(state, RunState::Streaming if !checkpoint.menu.blocking)
}

async fn latest_run_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
) -> Result<Option<RunState>, HaiderError> {
    let mut cursor = 0;
    let mut state = None;
    loop {
        let page = store
            .read_reducer_page(
                session_id,
                cursor,
                PAGE_SIZE,
                PAGE_BYTES,
                RUN_STATE_PAYLOAD_KINDS,
            )
            .await?;
        if page.is_empty() {
            return Ok(state);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id) || envelope.branch_id.as_ref() != branch_id
            {
                continue;
            }
            if let Ok(EventPayload::RunState(next)) = envelope.payload.decode_event() {
                state = Some(next);
            }
        }
    }
}

fn reduce(reductions: &mut HashMap<RunId, RunReduction>, envelope: &RawEnvelope) {
    let Some(run_id) = envelope.run_id.clone() else {
        return;
    };
    let payload = envelope.payload.decode_event().ok();
    let retry_payload =
        RunRetryEventPayload::from_payload_value(envelope.payload.to_json_value()).ok();
    let headless_payload = HeadlessRunEventPayload::from_payload_value(&envelope.payload);
    if payload.is_none() && retry_payload.is_none() && headless_payload.is_none() {
        return;
    }
    // SessionState is session-global even when it names the run that caused
    // the aggregate transition. Route it by payload type before enforcing
    // the immutable branch coordinate of run-scoped history.
    if matches!(payload.as_ref(), Some(EventPayload::SessionState(_))) {
        return;
    }
    let recovery_relevant = retry_payload.is_some()
        || matches!(
            headless_payload.as_ref(),
            Some(
                HeadlessRunEventPayload::HeadlessRunConfigured(_)
                    | HeadlessRunEventPayload::RunBudgetExhausted(_)
                    | HeadlessRunEventPayload::RunDeadlineExceeded(_)
            )
        )
        || matches!(
            payload.as_ref(),
            Some(
                EventPayload::RunState(_)
                    | EventPayload::UserMessage { .. }
                    | EventPayload::PeerMessage(_)
                    | EventPayload::Item(_)
                    | EventPayload::MenuOpened(_)
                    | EventPayload::MenuAnswered(_)
                    | EventPayload::MenuClosed { .. }
                    | EventPayload::ToolResult { .. }
                    | EventPayload::Usage(_)
                    | EventPayload::AgentReport(_)
                    | EventPayload::GraphFinalizationDeferred(_)
            )
        );
    if !recovery_relevant {
        return;
    }
    let reduction = reductions.entry(run_id).or_default();
    if reduction.branch_observed && reduction.branch_id != envelope.branch_id {
        reduction.branch_mismatch = true;
    } else if !reduction.branch_observed {
        reduction.branch_id = envelope.branch_id.clone();
        reduction.branch_observed = true;
    }
    if let Some(RunRetryEventPayload::RunRetried {
        failed_run_id,
        prompt_run_id,
        user_seq,
    }) = retry_payload
    {
        reduction.retry_source = Some((failed_run_id, prompt_run_id, user_seq, envelope.seq));
        return;
    }
    match headless_payload {
        Some(HeadlessRunEventPayload::HeadlessRunConfigured(configured))
            if envelope.render.durable =>
        {
            reduction.headless_configured = true;
            reduction.headless_budget = configured.budget;
            return;
        }
        Some(HeadlessRunEventPayload::RunBudgetExhausted(exhausted))
            if envelope.render.durable && reduction.headless_configured =>
        {
            reduction.budget_exhausted = Some(exhausted);
            return;
        }
        Some(
            HeadlessRunEventPayload::HeadlessRunConfigured(_)
            | HeadlessRunEventPayload::RunBudgetExhausted(_)
            | HeadlessRunEventPayload::RunDeadlineExceeded(_),
        ) => return,
        None => {}
    }
    let Some(payload) = payload else {
        return;
    };
    match payload {
        EventPayload::RunState(state) => {
            reduction.state = Some((state, envelope.seq));
            reduction.state_generation = envelope.worker_generation;
        }
        EventPayload::UserMessage { .. } | EventPayload::PeerMessage(_) => {
            reduction.user_seq = Some(envelope.seq);
        }
        EventPayload::Item(ItemEvent::Started { item_id, item }) => {
            if provider_response_item(&item) {
                reduction.latest_provider_response_seq = Some(envelope.seq);
            }
            if let TurnItem::ToolCall {
                call_id,
                name,
                args,
                ..
            } = &item
            {
                reduction.tool_calls.insert(
                    call_id.clone(),
                    RecoveredToolCall {
                        item_id: item_id.clone(),
                        name: name.clone(),
                        args: if args == &serde_json::json!({}) {
                            String::new()
                        } else {
                            args.to_string()
                        },
                        started_seq: envelope.seq,
                        completed: false,
                    },
                );
            }
            reduction.open_items.insert(
                item_id,
                OpenItem {
                    item,
                    started_seq: envelope.seq,
                    text: ReplyArenaWriter::new(),
                    args: String::new(),
                },
            );
        }
        EventPayload::Item(ItemEvent::Delta { item_id, delta }) => {
            if reduction
                .open_items
                .get(&item_id)
                .is_some_and(|open| provider_response_item(&open.item))
            {
                reduction.latest_provider_response_seq = Some(envelope.seq);
            }
            if let Some(open) = reduction.open_items.get_mut(&item_id) {
                match &delta {
                    ItemDelta::Text { text } => {
                        let _ = open.text.append_shared(text);
                        reduction
                            .route_replay_events
                            .push(StreamEvent::TextDelta { text: text.clone() });
                    }
                    ItemDelta::Reasoning { text } => {
                        let _ = open.text.append_shared(text);
                        reduction
                            .route_replay_events
                            .push(StreamEvent::ReasoningDelta { text: text.clone() });
                    }
                    ItemDelta::ToolArgs { fragment } => open.args.push_str(fragment),
                    ItemDelta::CommandOutput { .. } => {}
                }
            }
            if let ItemDelta::ToolArgs { fragment } = delta
                && let Some(tool) = reduction
                    .tool_calls
                    .values_mut()
                    .find(|tool| tool.item_id == item_id)
            {
                tool.args.push_str(&fragment);
            }
        }
        EventPayload::Item(ItemEvent::Completed { item_id, mut item }) => {
            if provider_response_item(&item) {
                reduction.latest_provider_response_seq = Some(envelope.seq);
            }
            match ProviderRequestAttemptV1::try_from_extension_item(&item) {
                Ok(Some(attempt)) => {
                    reduce_provider_request_attempt(
                        reduction,
                        envelope,
                        attempt.request_ordinal,
                        Some(attempt),
                        false,
                    );
                }
                Ok(None) => {}
                Err(_) => reduction.provider_request_attempt_corrupt = true,
            }
            match CacheRequestAttemptV1::try_from_extension_item(&item) {
                Ok(Some(attempt)) => {
                    reduce_provider_request_attempt(
                        reduction,
                        envelope,
                        attempt.ordinal,
                        attempt.correlation,
                        true,
                    );
                }
                Ok(None) => {}
                Err(_) => reduction.provider_request_attempt_corrupt = true,
            }
            if let Some(handoff) = DelegationMirrorHandoff::from_item(&item) {
                match handoff.phase {
                    DelegationMirrorHandoffPhase::Pending => {
                        reduction
                            .delegation_mirror_handoffs
                            .insert(handoff.handoff_id.clone(), handoff);
                    }
                    DelegationMirrorHandoffPhase::Completed => {
                        reduction
                            .delegation_mirror_handoffs
                            .remove(&handoff.handoff_id);
                    }
                }
                reduction.open_items.remove(&item_id);
                return;
            }
            if let TurnItem::Extension { kind, data } = &item {
                if kind == ROUTE_REPLAY_ATTEMPT_EXTENSION_KIND {
                    if let Some(epoch) = data
                        .get("response_epoch")
                        .and_then(serde_json::Value::as_u64)
                        && epoch != reduction.route_replay_epoch
                    {
                        reduction.route_replay_epoch = epoch;
                        reduction.route_replay_events.clear();
                    }
                } else if kind == ROUTE_REPLAY_EVENT_EXTENSION_KIND
                    && data
                        .get("response_epoch")
                        .and_then(serde_json::Value::as_u64)
                        == Some(reduction.route_replay_epoch)
                    && let Some(value) = data.get("stream_event")
                    && let Ok(event) = serde_json::from_value::<StreamEvent>(value.clone())
                {
                    reduction.route_replay_events.push(event);
                }
            }
            if let Some(open) = reduction.open_items.get(&item_id) {
                canonicalize_completed_reply(&mut item, open);
            }
            let item = match item {
                TurnItem::IncompleteAgentMessage { text, .. } => {
                    reduction.open_items.remove(&item_id);
                    reduction.incomplete_items.insert(item_id, text);
                    return;
                }
                item => item,
            };
            if let TurnItem::ToolCall {
                call_id,
                name,
                args,
                ..
            } = &item
            {
                let entry = reduction
                    .tool_calls
                    .entry(call_id.clone())
                    .or_insert_with(|| RecoveredToolCall {
                        item_id: item_id.clone(),
                        name: name.clone(),
                        args: args.to_string(),
                        started_seq: envelope.seq,
                        completed: true,
                    });
                entry.completed = true;
                if entry.args.is_empty() {
                    entry.args = args.to_string();
                }
            }
            if let TurnItem::ChildResult { report } = &item {
                reduction.child_results.insert(report.agent.clone());
            }
            reduction.open_items.remove(&item_id);
        }
        EventPayload::GraphFinalizationDeferred(deferred) => {
            reduction.workflow_deferral = Some((deferred, envelope.seq));
        }
        EventPayload::MenuOpened(menu) => {
            reduction.menu = Some(OpenMenu {
                menu,
                request_seq: envelope.seq,
                opening_generation: envelope.worker_generation,
            });
        }
        EventPayload::MenuAnswered(answer) => {
            reduction.menu_answers.insert(answer.menu, envelope.clone());
        }
        EventPayload::MenuClosed { menu, .. }
            if reduction
                .menu
                .as_ref()
                .is_some_and(|open| open.menu.id == menu) =>
        {
            reduction.menu = None;
        }
        EventPayload::ToolResult { call_id, result } => {
            reduction.tool_results.insert(call_id.clone());
            reduction.tool_result_values.insert(call_id, result);
        }
        EventPayload::Usage(_) => {
            reduction.latest_provider_response_seq = Some(envelope.seq);
        }
        EventPayload::AgentReport(report) => {
            reduction.agent_reports.insert(report.agent);
        }
        _ => {}
    }
}

fn reduce_provider_request_attempt(
    reduction: &mut RunReduction,
    envelope: &RawEnvelope,
    ordinal: u64,
    correlation: Option<ProviderRequestAttemptV1>,
    model_boundary: bool,
) {
    let invalid = ordinal == 0
        || correlation.as_ref().is_some_and(|correlation| {
            correlation.request_ordinal != ordinal
                || correlation.session_id != envelope.session_id
                || envelope.run_id.as_ref() != Some(&correlation.run_id)
                || !correlation.coordinates_valid()
                || reduction
                    .provider_request_turn_ordinal
                    .is_some_and(|turn_ordinal| turn_ordinal != correlation.turn_ordinal)
                || recovered_provider_request_ordinal(reduction) >= ordinal
        });
    if invalid {
        reduction.provider_request_attempt_corrupt = true;
        return;
    }
    reduction.provider_request_ordinal_max = reduction.provider_request_ordinal_max.max(ordinal);
    if model_boundary {
        reduction.provider_model_request_attempt_count = reduction
            .provider_model_request_attempt_count
            .saturating_add(1);
    }
    if model_boundary
        && reduction
            .latest_provider_request_attempt
            .is_none_or(|(_, latest)| ordinal > latest)
    {
        reduction.latest_provider_request_attempt = Some((envelope.seq, ordinal));
    }
    if let Some(correlation) = correlation {
        reduction.provider_request_turn_ordinal = Some(correlation.turn_ordinal);
    }
}

fn recovered_provider_request_ordinal(reduction: &RunReduction) -> u64 {
    reduction.provider_request_ordinal_max.max(
        reduction
            .latest_provider_request_attempt
            .map_or(0, |(_, ordinal)| ordinal),
    )
}

fn provider_response_item(item: &TurnItem) -> bool {
    matches!(
        item,
        TurnItem::AgentMessage { .. }
            | TurnItem::IncompleteAgentMessage { .. }
            | TurnItem::Reasoning { .. }
            | TurnItem::ToolCall { .. }
            | TurnItem::Refusal { .. }
    ) || matches!(
        item,
        TurnItem::Extension { kind, .. }
            if kind == PROVIDER_OPAQUE_EXTENSION_KIND
                || kind == ROUTE_REPLAY_EVENT_EXTENSION_KIND
    )
}

fn canonicalize_completed_reply(item: &mut TurnItem, open: &OpenItem) {
    let canonical = open.text.snapshot();
    let completed = match item {
        TurnItem::AgentMessage { text } | TurnItem::IncompleteAgentMessage { text, .. } => text,
        TurnItem::Reasoning { summary } => summary,
        _ => return,
    };
    if *completed == canonical {
        *completed = canonical;
    }
}

fn pending_checkpoint(reduction: &RunReduction) -> Option<RequestInputCheckpoint> {
    let open_menu = reduction.menu.clone()?;
    reduction
        .open_items
        .iter()
        .find_map(|(item_id, open)| match &open.item {
            TurnItem::ToolCall { call_id, name, .. }
                if !reduction.tool_results.contains(call_id)
                    && match &open_menu.menu.kind {
                        haider_protocol::menu::MenuKind::Permission { .. } => {
                            name != "request_input" && name != "plan"
                        }
                        // `request_input` parks; an interrupted autonomous
                        // `plan` uses the same checkpoint carrier so recovery
                        // can journal its acceptance and continue.
                        _ => name == "request_input" || name == "plan",
                    } =>
            {
                Some(RequestInputCheckpoint {
                    menu: open_menu.menu.clone(),
                    request_seq: open_menu.request_seq,
                    opening_generation: open_menu.opening_generation,
                    tool_item_id: item_id.clone(),
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                    args: open.args.clone(),
                })
            }
            _ => None,
        })
}

fn pending_partial_stream_checkpoint(reduction: &RunReduction) -> Option<PartialStreamCheckpoint> {
    let open_menu = reduction.menu.clone()?;
    let MenuKind::ErrorRecovery {
        card: haider_protocol::menu::ErrorRecoveryCardKind::PartialStream,
        source_item: Some(item_id),
        ..
    } = &open_menu.menu.kind
    else {
        return None;
    };
    let item_id = item_id.clone();
    let text = reduction.incomplete_items.get(&item_id)?;
    Some(PartialStreamCheckpoint {
        menu: open_menu.menu,
        request_seq: open_menu.request_seq,
        opening_generation: open_menu.opening_generation,
        item_id,
        text: text.clone(),
    })
}

fn pending_route_wait_checkpoint(reduction: &RunReduction) -> Option<RouteWaitCheckpoint> {
    let mut checkpoint = RouteWaitCheckpoint {
        structured_events: reduction.route_replay_events.clone(),
        response_epoch: reduction.route_replay_epoch,
        ..RouteWaitCheckpoint::default()
    };
    let mut tools = Vec::new();
    for (item_id, open) in &reduction.open_items {
        let text = RouteWaitTextCheckpoint {
            item_id: item_id.clone(),
            text: open.text.snapshot(),
        };
        match &open.item {
            TurnItem::AgentMessage { .. } if checkpoint.message.is_none() => {
                checkpoint.message = Some(text);
            }
            TurnItem::Reasoning { .. } if checkpoint.reasoning.is_none() => {
                checkpoint.reasoning = Some(text);
            }
            TurnItem::ToolCall { call_id, name, .. } => {
                tools.push((
                    open.started_seq,
                    RouteWaitToolCheckpoint {
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args: open.args.clone(),
                    },
                ));
            }
            _ => return None,
        }
    }
    tools.sort_by_key(|(started_seq, _)| *started_seq);
    checkpoint.tools = tools
        .into_iter()
        .map(|(_, checkpoint)| checkpoint)
        .collect();
    let completed_call_ids = checkpoint
        .structured_events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallEnd { call_id } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut completed_tools = reduction
        .tool_calls
        .iter()
        .filter(|(call_id, tool)| tool.completed && completed_call_ids.contains(call_id.as_str()))
        .map(|(call_id, tool)| {
            (
                tool.started_seq,
                RouteWaitCompletedToolCheckpoint {
                    call_id: call_id.clone(),
                    name: tool.name.clone(),
                    args: serde_json::from_str(&tool.args)
                        .unwrap_or_else(|_| serde_json::Value::String(tool.args.clone())),
                    result: reduction.tool_result_values.get(call_id).cloned(),
                },
            )
        })
        .collect::<Vec<_>>();
    completed_tools.sort_by_key(|(started_seq, _)| *started_seq);
    checkpoint.completed_tools = completed_tools
        .into_iter()
        .map(|(_, checkpoint)| checkpoint)
        .collect();
    Some(checkpoint)
}

async fn pending_child_wait(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    reduction: &RunReduction,
) -> Result<Option<ChildWaitCheckpoint>, HaiderError> {
    let delegations = store
        .delegations_for_parent_run(session_id.clone(), run_id.clone())
        .await?;
    if delegations.is_empty() {
        return Ok(None);
    }
    let mut checkpoints = Vec::with_capacity(delegations.len());
    for delegation in delegations {
        if delegation.parent_branch_id != reduction.branch_id {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!(
                    "delegation {} crosses parent branch scopes",
                    delegation.agent_id
                ),
                false,
            ));
        }
        let tool = reduction
            .tool_calls
            .get(&delegation.call_id)
            .filter(|tool| tool.item_id == delegation.tool_item_id)
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "delegation {} has no matching parent spawn tool",
                        delegation.agent_id
                    ),
                    false,
                )
            })?;
        checkpoints.push((
            tool.started_seq,
            DeferredToolCheckpoint {
                ticket: DeferredTicket {
                    id: delegation.agent_id.as_str().to_owned(),
                    manifest: delegation.manifest,
                },
                tool_item_id: tool.item_id.clone(),
                call_id: delegation.call_id.clone(),
                tool_name: tool.name.clone(),
                args: tool.args.clone(),
                report_emitted: reduction.agent_reports.contains(&delegation.agent_id),
                child_result_emitted: reduction.child_results.contains(&delegation.agent_id),
                tool_result_emitted: reduction.tool_results.contains(&delegation.call_id),
                item_completed: tool.completed,
            },
        ));
    }
    checkpoints.sort_by_key(|(started_seq, _)| *started_seq);
    Ok(Some(ChildWaitCheckpoint {
        tools: checkpoints
            .into_iter()
            .map(|(_, checkpoint)| checkpoint)
            .collect(),
    }))
}

fn recovered_acceptance(
    session_id: &SessionId,
    run_id: &RunId,
    turn_ordinal: u64,
    accepted_seq: u64,
    worker_generation: u64,
    branch_id: Option<BranchId>,
) -> AcceptedTurn {
    AcceptedTurn {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        turn_ordinal,
        accepted_seq,
        worker_generation,
        branch_id,
        disposition: TurnAdmissionDisposition::Started,
        // Recovery reconstructions never re-title (G2): the auto-title
        // consumed its own durable receipt at the original acceptance.
        first_user_turn: false,
        pdf_attachments: Vec::new(),
    }
}

async fn terminalize_interrupted(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    session_id: &SessionId,
    run_id: &RunId,
    branch_id: Option<BranchId>,
    reduction: RunReduction,
    cancelling: bool,
) -> Result<(), HaiderError> {
    let (cancelling, code, message, retryable) = interrupted_restart_cause(&reduction, cancelling);
    let payloads = interrupted_terminal_payloads(reduction, cancelling, code, message, retryable);
    let mut envelopes = recovery_envelopes(
        store.worker_generation(),
        device_id,
        session_id,
        run_id,
        branch_id.as_ref(),
        payloads,
    )?;
    store.append(&mut envelopes).await?;
    Ok(())
}

fn interrupted_restart_cause(
    reduction: &RunReduction,
    cancelling: bool,
) -> (bool, ErrorCode, String, bool) {
    match reduction.budget_exhausted.as_ref() {
        Some(exhausted) => (
            false,
            ErrorCode::BudgetExhausted,
            exhausted.summary(),
            false,
        ),
        None => (
            cancelling,
            ErrorCode::Internal,
            "run was interrupted by daemon restart".into(),
            true,
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn interrupted_recovery_payloads_for_test(
    run_id: &RunId,
    envelopes: &[RawEnvelope],
    cancelling: bool,
) -> Vec<EventPayload> {
    let mut reductions = HashMap::new();
    for envelope in envelopes {
        reduce(&mut reductions, envelope);
    }
    let reduction = reductions.remove(run_id).unwrap_or_default();
    let (cancelling, code, message, retryable) = interrupted_restart_cause(&reduction, cancelling);
    interrupted_terminal_payloads(reduction, cancelling, code, message, retryable)
}

pub(crate) async fn failed_resumption_payloads(
    store: &impl StoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    error: &HaiderError,
) -> Result<Vec<EventPayload>, HaiderError> {
    resumption_terminal_payloads(store, session_id, run_id, Some(error)).await
}

/// Builds cancellation-shaped closure for every run-local lifecycle object.
///
/// This is shared by startup cancellation and live supervisor-exit recovery:
/// a durable `Cancelling` rejects failure-shaped state transitions, but open
/// items and menus must still close before the final `Cancelled` envelope.
pub(crate) async fn cancelled_resumption_payloads(
    store: &impl StoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Vec<EventPayload>, HaiderError> {
    resumption_terminal_payloads(store, session_id, run_id, None).await
}

async fn resumption_terminal_payloads(
    store: &impl StoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
    error: Option<&HaiderError>,
) -> Result<Vec<EventPayload>, HaiderError> {
    let mut reduction = RunReduction::default();
    let mut cursor = 0;
    loop {
        let page = store.read(session_id, cursor, PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in &page {
            if envelope.run_id.as_ref() == Some(run_id) {
                let mut reductions = HashMap::from([(run_id.clone(), reduction)]);
                reduce(&mut reductions, envelope);
                reduction = reductions.remove(run_id).unwrap_or_default();
            }
        }
    }
    let (mut cancelling, mut failure_code, mut failure_message, mut retryable) = match error {
        Some(error) => (
            false,
            error.code,
            haider_core::sanitized_failure_message(&error.message),
            error.retryable,
        ),
        None => (true, ErrorCode::Internal, "run was cancelled".into(), false),
    };
    if reduction.branch_mismatch {
        return Err(HaiderError::new(
            ErrorCode::StoreCorrupt,
            format!("run {run_id} crosses branch scopes"),
            false,
        ));
    }
    // E6: supervisor loss may have parked a dispatched effect behind the
    // standard reconciliation card. Do not immediately erase that honest
    // state with the generic interrupted-run terminalizer. The committed
    // menu answer and reconciliation handler now own closure.
    if matches!(
        reduction.state.as_ref().map(|(state, _)| state),
        Some(RunState::EffectOutcomeUnknown)
    ) {
        return Ok(Vec::new());
    }
    if let Some(exhausted) = reduction.budget_exhausted.as_ref() {
        cancelling = false;
        failure_code = ErrorCode::BudgetExhausted;
        failure_message = exhausted.summary();
        retryable = false;
    }
    Ok(interrupted_terminal_payloads(
        reduction,
        cancelling,
        failure_code,
        failure_message,
        retryable,
    ))
}

fn interrupted_terminal_payloads(
    reduction: RunReduction,
    cancelling: bool,
    failure_code: ErrorCode,
    failure_message: String,
    retryable: bool,
) -> Vec<EventPayload> {
    let terminal = if cancelling {
        RunState::Cancelled
    } else {
        RunState::Errored
    };
    let tool_status = if cancelling {
        ToolStatus::Cancelled
    } else {
        ToolStatus::Failed
    };
    let mut payloads = Vec::new();
    let mut open_items = reduction.open_items.into_iter().collect::<Vec<_>>();
    open_items.sort_by_key(|(_, open)| open.started_seq);
    for (item_id, open) in open_items {
        let item = match open.item {
            TurnItem::AgentMessage { .. } => TurnItem::AgentMessage {
                text: open.text.snapshot(),
            },
            TurnItem::Reasoning { .. } => TurnItem::Reasoning {
                summary: open.text.snapshot(),
            },
            TurnItem::ToolCall {
                call_id,
                name,
                args,
                ..
            } => TurnItem::ToolCall {
                call_id,
                name,
                args: if open.args.is_empty() {
                    args
                } else {
                    serde_json::from_str(&open.args).unwrap_or(serde_json::Value::String(open.args))
                },
                status: tool_status,
            },
            TurnItem::CommandExecution {
                call_id,
                command,
                exit_code,
                ..
            } => TurnItem::CommandExecution {
                call_id,
                command,
                status: tool_status,
                exit_code,
            },
            item => item,
        };
        payloads.push(EventPayload::Item(ItemEvent::Completed { item_id, item }));
    }
    if let Some(open_menu) = reduction.menu {
        payloads.push(EventPayload::MenuClosed {
            menu: open_menu.menu.id,
            reason: if cancelling {
                MenuCloseReason::Cancelled
            } else {
                MenuCloseReason::RecoveryInterrupted
            },
        });
    }
    if !cancelling {
        payloads.push(EventPayload::RunFailed {
            code: failure_code,
            message: failure_message,
            retryable,
            presentation: Some(ErrorPresentation::new(
                "run-recovery-interrupted",
                "Interrupted run could not resume",
                "Haider recovered the journal but could not safely resume this run.",
                ErrorScope::Turn,
                [if retryable {
                    ErrorAction::RetryFresh
                } else {
                    ErrorAction::None
                }],
            )),
        });
    }
    payloads.push(EventPayload::RunState(terminal));
    payloads.push(EventPayload::SessionState(SessionState::Idle {
        interrupted: true,
    }));
    payloads
}

async fn append_recovered_active(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<(), HaiderError> {
    let mut envelopes = recovery_envelopes(
        store.worker_generation(),
        device_id,
        session_id,
        run_id,
        None,
        vec![EventPayload::SessionState(SessionState::ActiveRun)],
    )?;
    store.append(&mut envelopes).await?;
    Ok(())
}

fn recovery_envelopes(
    generation: u64,
    device_id: &DeviceId,
    session_id: &SessionId,
    run_id: &RunId,
    branch_id: Option<&BranchId>,
    payloads: Vec<EventPayload>,
) -> Result<Vec<RawEnvelope>, HaiderError> {
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            let is_session = matches!(payload, EventPayload::SessionState(_));
            Ok(EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!(
                    "turn-recovery-{}-{}-{}-{}",
                    generation,
                    session_id,
                    run_id,
                    index + 1
                )),
                seq: 0,
                session_id: session_id.clone(),
                branch_id: if is_session { None } else { branch_id.cloned() },
                run_id: (!is_session).then(|| run_id.clone()),
                agent_id: None,
                device_id: device_id.clone(),
                authority_epoch: 0,
                worker_generation: generation,
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: RawPayload::from_event(payload).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("cannot serialize turn recovery payload: {error}"),
                        false,
                    )
                })?,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "turn_recovery_streaming_checkpoint_tests.rs"]
mod streaming_checkpoint_tests;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod composite_recovery_tests {
    use super::*;

    #[test]
    fn auxiliary_attempt_advances_identity_without_moving_model_boundary() {
        let session_id = SessionId::new("turnid-session");
        let run_id = RunId::new("turnid-run");
        let mut envelopes = recovery_envelopes(
            1,
            &DeviceId::new("turnid-device"),
            &session_id,
            &run_id,
            None,
            vec![EventPayload::RunState(RunState::Thinking)],
        )
        .expect("recovery envelope");
        let envelope = envelopes.first_mut().expect("one envelope");
        envelope.seq = 7;

        let attempt = |request_ordinal, request_kind| ProviderRequestAttemptV1 {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_ordinal: 1,
            request_ordinal,
            request_kind,
        };
        let mut reduction = RunReduction::default();
        reduce_provider_request_attempt(
            &mut reduction,
            envelope,
            1,
            Some(attempt(
                1,
                haider_protocol::cache::ProviderRequestKind::Primary,
            )),
            true,
        );
        envelope.seq = 8;
        reduce_provider_request_attempt(
            &mut reduction,
            envelope,
            2,
            Some(attempt(
                2,
                haider_protocol::cache::ProviderRequestKind::Side,
            )),
            false,
        );

        assert_eq!(reduction.latest_provider_request_attempt, Some((7, 1)));
        assert_eq!(recovered_provider_request_ordinal(&reduction), 2);
        assert!(!reduction.provider_request_attempt_corrupt);

        // A physical warmup may be the first HTTP request without being the
        // first model boundary. Admission recovery must still recognize the
        // primary request and resume from the physical maximum.
        let mut admission = RunReduction {
            state: Some((RunState::Thinking, 5)),
            user_seq: Some(1),
            headless_configured: true,
            ..RunReduction::default()
        };
        envelope.seq = 6;
        reduce_provider_request_attempt(
            &mut admission,
            envelope,
            1,
            Some(attempt(
                1,
                haider_protocol::cache::ProviderRequestKind::Warmup,
            )),
            false,
        );
        envelope.seq = 7;
        reduce_provider_request_attempt(
            &mut admission,
            envelope,
            2,
            Some(attempt(
                2,
                haider_protocol::cache::ProviderRequestKind::Primary,
            )),
            true,
        );
        assert_eq!(admission.latest_provider_request_attempt, Some((7, 2)));
        assert_eq!(admission.provider_model_request_attempt_count, 1);
        assert_eq!(recovered_provider_request_ordinal(&admission), 2);
        assert!(admitted_first_request_has_no_response(
            &RunState::Thinking,
            &admission
        ));
        assert_eq!(
            recovered_provider_request_ordinal(&admission).checked_add(1),
            Some(3)
        );
    }

    /// MUTATION CHECK: omit the response coordinate or accept a later
    /// response fact and the final assertion flips true, allowing a daemon
    /// restart to replay provider work after response delivery began.
    #[test]
    fn admitted_first_request_retries_only_before_any_response_fact() {
        let mut reduction = RunReduction {
            state: Some((RunState::Thinking, 5)),
            user_seq: Some(1),
            headless_configured: true,
            latest_provider_request_attempt: Some((6, 1)),
            provider_model_request_attempt_count: 1,
            ..RunReduction::default()
        };
        assert!(admitted_first_request_has_no_response(
            &RunState::Thinking,
            &reduction
        ));

        reduction.state = Some((RunState::Streaming, 7));
        assert!(admitted_first_request_has_no_response(
            &RunState::Streaming,
            &reduction
        ));

        reduction.latest_provider_response_seq = Some(8);
        assert!(!admitted_first_request_has_no_response(
            &RunState::Streaming,
            &reduction
        ));
    }

    /// Cross-contract pin: the max-cost workflow recovery test owns the
    /// budgeted side of this split, while kill9_midturn owns the unbudgeted
    /// effect-outcome-unknown side. Removing either discriminator makes one
    /// of those real-process/restart contracts fail.
    #[test]
    fn only_budgeted_active_workflow_admission_is_runnable_recovery_work() {
        let unbudgeted = RunReduction::default();
        assert!(!budgeted_workflow_admission_is_recoverable(
            &unbudgeted,
            Some(GraphPhase::Active)
        ));

        // Request tranches must not widen the existing ambiguous-dispatch
        // recovery policy. Only the shared token/cost/time coordinator owns it.
        let request_only = RunReduction {
            headless_budget: RunBudgetV1 {
                request_budget: Some(Default::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!budgeted_workflow_admission_is_recoverable(
            &request_only,
            Some(GraphPhase::Active),
        ));
        let budgeted = RunReduction {
            headless_budget: RunBudgetV1 {
                max_cost_microusd: Some(10_000_000),
                ..RunBudgetV1::default()
            },
            ..RunReduction::default()
        };
        assert!(!budgeted_workflow_admission_is_recoverable(&budgeted, None));
        assert!(!budgeted_workflow_admission_is_recoverable(
            &budgeted,
            Some(GraphPhase::Completed)
        ));
        assert!(budgeted_workflow_admission_is_recoverable(
            &budgeted,
            Some(GraphPhase::Active)
        ));
    }

    /// CROSS-LANE MUTATION CHECK: remove the pending-mirror condition from
    /// `workflow_continuation_shape_is_eligible`. The assertion flips true,
    /// proving durable child cancellation owns recovery before wfcont can
    /// restore logical-request and physical-attempt coordinates.
    #[test]
    fn pending_cancellation_handoff_suppresses_workflow_continuation_shape() {
        let run_id = RunId::new("composite-recovery-run");
        let mut reduction = RunReduction {
            state: Some((RunState::Streaming, 12)),
            state_generation: 4,
            user_seq: Some(1),
            headless_configured: true,
            workflow_deferral: Some((
                GraphFinalizationDeferred {
                    graph_id: haider_protocol::ids::GraphId::new("composite-recovery-graph"),
                    run_id: run_id.clone(),
                    state_digest: "composite-state".into(),
                    provider_requests_consumed: 3,
                    unmet_nodes: vec![
                        haider_protocol::graph::GraphNodeName::new("VERIFY").expect("valid node"),
                    ],
                },
                11,
            )),
            latest_provider_request_attempt: Some((10, 7)),
            latest_provider_response_seq: Some(9),
            ..RunReduction::default()
        };
        let handoff = DelegationMirrorHandoff {
            handoff_id: "composite-handoff".into(),
            agent: AgentId::new("composite-agent"),
            child_session_id: SessionId::new("composite-child"),
            child_run_id: run_id,
            deadline_at_ms: 241_000,
            cancel_cause: "parent".into(),
            source: "composite-test".into(),
            phase: DelegationMirrorHandoffPhase::Pending,
        };
        reduction
            .delegation_mirror_handoffs
            .insert(handoff.handoff_id.clone(), handoff.clone());

        let encoded = rmp_serde::to_vec(&reduction).expect("encode composite checkpoint");
        let recovered: RunReduction =
            rmp_serde::from_slice(&encoded).expect("decode composite checkpoint");
        assert_eq!(CHECKPOINT_REDUCER_VERSION, "startup-turn-recovery-v8");
        assert_eq!(
            recovered
                .workflow_deferral
                .as_ref()
                .map(|(deferred, _)| deferred.provider_requests_consumed),
            Some(3)
        );
        assert_eq!(recovered.latest_provider_request_attempt, Some((10, 7)));
        assert_eq!(recovered.latest_provider_response_seq, Some(9));
        assert_eq!(
            recovered
                .delegation_mirror_handoffs
                .get("composite-handoff"),
            Some(&handoff)
        );
        assert!(!workflow_continuation_shape_is_eligible(
            &RunState::Streaming,
            &recovered
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod partial_stream_recovery_tests {
    use super::*;
    use haider_protocol::error::ErrorAction;
    use haider_protocol::menu::{ErrorRecoveryCardKind, MenuOption, MenuScope};

    #[test]
    fn e4_restart_reconstructs_partial_stream_checkpoint_from_durable_marker() {
        let session_id = SessionId::new("partial-restart");
        let run_id = RunId::new("run-partial-restart");
        let item_id = ItemId::new("partial-item");
        let menu_id = MenuId::new("partial-menu");
        let presentation = ErrorPresentation::new(
            "stream-interrupted",
            "Response interrupted",
            "The provider stream ended before the response completed.",
            ErrorScope::Turn,
            [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
        );
        let menu = Menu {
            id: menu_id.clone(),
            kind: MenuKind::ErrorRecovery {
                card: ErrorRecoveryCardKind::PartialStream,
                presentation: presentation.clone(),
                option_actions: vec![ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
                provider: Some("fake".into()),
                account: None,
                source_run: Some(run_id.clone()),
                source_item: Some(item_id.clone()),
            },
            title: presentation.title.clone(),
            body: vec![presentation.detail.clone()],
            options: vec![
                MenuOption {
                    key: "continue_partial".into(),
                    label: "Continue from partial".into(),
                    detail: None,
                    decision: None,
                },
                MenuOption {
                    key: "retry_fresh".into(),
                    label: "Retry from scratch".into(),
                    detail: None,
                    decision: None,
                },
            ],
            blocking: true,
            scope: MenuScope::Session,
            origin: "provider".into(),
            ttl_ms: None,
            timeout_option: None,
        };
        let payloads = vec![
            EventPayload::UserMessage {
                text: "question".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
            EventPayload::Item(ItemEvent::Completed {
                item_id: item_id.clone(),
                item: TurnItem::IncompleteAgentMessage {
                    text: "durable partial".into(),
                    interruption: presentation,
                },
            }),
            EventPayload::MenuOpened(menu),
            EventPayload::RunState(RunState::InputRequired {
                menu: menu_id.clone(),
            }),
        ];
        let mut envelopes = recovery_envelopes(
            7,
            &DeviceId::new("restart-device"),
            &session_id,
            &run_id,
            None,
            payloads,
        )
        .expect("recovery envelopes");
        for (index, envelope) in envelopes.iter_mut().enumerate() {
            envelope.seq = u64::try_from(index + 1).expect("small sequence");
        }
        let mut reductions = HashMap::new();
        for envelope in &envelopes {
            reduce(&mut reductions, envelope);
        }
        let reduction = reductions.get(&run_id).expect("reduced run");
        let checkpoint = pending_partial_stream_checkpoint(reduction).expect("partial checkpoint");
        assert_eq!(checkpoint.item_id, item_id);
        assert_eq!(checkpoint.text, "durable partial");
        assert_eq!(checkpoint.menu.id, menu_id);
        assert_eq!(checkpoint.request_seq, 3);
        assert_eq!(checkpoint.opening_generation, 7);
        assert!(partial_stream_checkpoint_state_matches(
            &RunState::InputRequired {
                menu: menu_id.clone(),
            },
            &checkpoint,
        ));
        let mut autonomous_checkpoint = checkpoint.clone();
        autonomous_checkpoint.menu.blocking = false;
        assert!(partial_stream_checkpoint_state_matches(
            &RunState::Streaming,
            &autonomous_checkpoint,
        ));
        assert_eq!(
            checkpoint.menu.kind,
            MenuKind::ErrorRecovery {
                card: ErrorRecoveryCardKind::PartialStream,
                presentation: ErrorPresentation::new(
                    "stream-interrupted",
                    "Response interrupted",
                    "The provider stream ended before the response completed.",
                    ErrorScope::Turn,
                    [ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
                ),
                option_actions: vec![ErrorAction::ContinuePartial, ErrorAction::RetryFresh],
                provider: Some("fake".into()),
                account: None,
                source_run: Some(run_id),
                source_item: Some(item_id),
            }
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod plan_recovery_tests {
    use super::*;
    use haider_protocol::item::ToolStatus;
    use haider_protocol::menu::{MenuOption, MenuScope};

    /// D4 MUTATION CHECK: require InputRequired or narrow the pending
    /// checkpoint predicate back to `request_input` only. Expected RUNTIME
    /// failure: a daemon restart between plan presentation and automatic
    /// acceptance would drop the continuing turn.
    #[test]
    fn restart_reconstructs_an_interrupted_autonomous_plan_checkpoint() {
        let session_id = SessionId::new("plan-restart");
        let run_id = RunId::new("run-plan-restart");
        let item_id = ItemId::new("plan-item");
        let menu_id = MenuId::new("plan-menu");
        let plan = haider_tools::Plan {
            title: "Datacenter build-out".into(),
            body: "# Tiers\n\n- edge".into(),
        };
        let menu = {
            let mut menu = plan.menu(menu_id.clone());
            menu.id = menu_id.clone();
            menu
        };
        let args = serde_json::json!({
            "title": "Datacenter build-out",
            "body": "# Tiers\n\n- edge",
        });
        let payloads = vec![
            EventPayload::UserMessage {
                text: "propose it".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: "plan-call".into(),
                    name: "plan".into(),
                    args: args.clone(),
                    status: ToolStatus::InProgress,
                },
            }),
            EventPayload::MenuOpened(menu),
            EventPayload::RunState(RunState::Streaming),
        ];
        let mut envelopes = recovery_envelopes(
            7,
            &DeviceId::new("plan-restart-device"),
            &session_id,
            &run_id,
            None,
            payloads,
        )
        .expect("recovery envelopes");
        for (index, envelope) in envelopes.iter_mut().enumerate() {
            envelope.seq = u64::try_from(index + 1).expect("small sequence");
        }
        let mut reductions = HashMap::new();
        for envelope in &envelopes {
            reduce(&mut reductions, envelope);
        }
        let reduction = reductions.get(&run_id).expect("reduced run");
        let checkpoint = pending_checkpoint(reduction).expect("plan checkpoint reconstructs");
        assert_eq!(checkpoint.tool_name, "plan");
        assert_eq!(checkpoint.call_id, "plan-call");
        assert_eq!(checkpoint.menu.id, menu_id);
        assert_eq!(checkpoint.menu.origin, "plan");
        assert!(!checkpoint.menu.blocking);
        assert!(checkpoint_state_matches(&RunState::Streaming, &checkpoint));
        // The durable menu still carries the full document for the client.
        assert_eq!(checkpoint.menu.body[0], "# Tiers");
        let _ = (
            MenuOption {
                key: String::new(),
                label: String::new(),
                detail: None,
                decision: None,
            },
            MenuScope::Session,
        );
    }

    #[test]
    fn restart_reconstructs_an_autonomous_request_input_checkpoint() {
        let session_id = SessionId::new("request-input-restart");
        let run_id = RunId::new("run-request-input-restart");
        let item_id = ItemId::new("request-input-item");
        let menu_id = MenuId::new("request-input-menu");
        let args = serde_json::json!({
            "kind": "choice",
            "title": "Choose target",
            "options": [{"key": "only", "label": "Only option"}]
        });
        let mut menu = haider_tools::RequestInput::from_tool_args(args.clone())
            .expect("valid request")
            .menu(menu_id.clone());
        menu.blocking = false;
        let payloads = vec![
            EventPayload::UserMessage {
                text: "continue headlessly".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: "request-input-call".into(),
                    name: "request_input".into(),
                    args: args.clone(),
                    status: ToolStatus::InProgress,
                },
            }),
            EventPayload::MenuOpened(menu),
            EventPayload::RunState(RunState::Streaming),
        ];
        let mut envelopes = recovery_envelopes(
            7,
            &DeviceId::new("request-input-restart-device"),
            &session_id,
            &run_id,
            None,
            payloads,
        )
        .expect("recovery envelopes");
        for (index, envelope) in envelopes.iter_mut().enumerate() {
            envelope.seq = u64::try_from(index + 1).expect("small sequence");
        }
        let mut reductions = HashMap::new();
        for envelope in &envelopes {
            reduce(&mut reductions, envelope);
        }
        let reduction = reductions.get(&run_id).expect("reduced run");
        let checkpoint = pending_checkpoint(reduction).expect("request checkpoint reconstructs");
        assert_eq!(checkpoint.tool_name, "request_input");
        assert_eq!(checkpoint.call_id, "request-input-call");
        assert_eq!(checkpoint.menu.id, menu_id);
        assert!(!checkpoint.menu.blocking);
        assert!(checkpoint_state_matches(&RunState::Streaming, &checkpoint));
    }
}
