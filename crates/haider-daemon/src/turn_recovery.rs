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
//! - an active delegated child is left nonterminal for its recovered parent's
//!   durable child-wait coordinator. W6c re-arms the progress deadline from
//!   committed envelope time, delivers at most one steer, and then uses the
//!   ordinary durable cancel path; provider/tool work is never redispatched;
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

use haider_core::{
    AcceptedRunRetry, AcceptedTurn, ChildWaitCheckpoint, DeferredTicket, DeferredToolCheckpoint,
    PartialStreamCheckpoint, RequestInputCheckpoint, SessionProjectionCheckpoint,
    SqliteStoreHandle, StoreHandle, TurnAdmissionDisposition,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, ItemId, MenuId, RunId, SessionId,
};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason, MenuKind};
use haider_protocol::retry::RunRetryEventPayload;
use haider_protocol::state::{RunState, SessionState, WaitReason};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const PAGE_SIZE: usize = 512;
const PAGE_BYTES: usize = 4 * 1_024 * 1_024;
const RUN_STATE_PAYLOAD_KINDS: &[&str] = &["run_state"];
pub(crate) const STARTUP_HYDRATION_PAYLOAD_KINDS: &[&str] = &[
    "agent_report",
    "effect",
    "hook_run_trust",
    "item",
    "item_tool_call",
    "menu_answered",
    "menu_closed",
    "menu_opened",
    "node_committed",
    "run_failed",
    "run_retried",
    "run_state",
    "tool_result",
    "user_message",
];
const CHECKPOINT_PROJECTION: &str = "startup_turn_recovery";
const CHECKPOINT_TIMELINE: &str = "session";
const CHECKPOINT_SHAPE_VERSION: u32 = 1;
const CHECKPOINT_REDUCER_VERSION: &str = "startup-turn-recovery-v1";

pub(crate) enum RecoveredWork {
    Queued(AcceptedTurn),
    Retry(AcceptedRunRetry),
    Checkpoint(Box<RecoveredCheckpoint>),
    PartialStream(Box<RecoveredPartialStream>),
    ChildWait(Box<RecoveredChildWait>),
}

pub(crate) struct RecoveredPartialStream {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: PartialStreamCheckpoint,
    pub(crate) committed_answer: Option<RawEnvelope>,
}

pub(crate) struct RecoveredChildWait {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: ChildWaitCheckpoint,
}

pub(crate) struct RecoveredCheckpoint {
    pub(crate) accepted: AcceptedTurn,
    pub(crate) checkpoint: RequestInputCheckpoint,
    pub(crate) committed_answer: Option<RawEnvelope>,
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
    incomplete_items: HashMap<ItemId, String>,
    menu: Option<OpenMenu>,
    menu_answers: HashMap<MenuId, RawEnvelope>,
    tool_results: HashSet<String>,
    tool_calls: HashMap<String, RecoveredToolCall>,
    agent_reports: HashSet<AgentId>,
    child_results: HashSet<AgentId>,
}

#[derive(Serialize, Deserialize)]
struct RecoveredToolCall {
    item_id: ItemId,
    name: String,
    args: String,
    started_seq: u64,
    completed: bool,
}

#[derive(Serialize, Deserialize)]
struct OpenItem {
    item: TurnItem,
    started_seq: u64,
    text: String,
    args: String,
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
            let Some((state, _)) = reduction.state.clone() else {
                continue;
            };
            if state.is_terminal() || reduction.state_generation == store.worker_generation() {
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
            if state == RunState::Queued {
                if let Some(accepted_seq) = reduction.user_seq {
                    if !activated_recovered_queue {
                        append_recovered_active(store, device_id, &session_id, &run_id).await?;
                        touched = true;
                        activated_recovered_queue = true;
                    }
                    recovered.push(RecoveredWork::Queued(recovered_acceptance(
                        &session_id,
                        &run_id,
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    )));
                } else if let Some((failed_run_id, prompt_run_id, user_seq, accepted_seq)) =
                    reduction.retry_source
                {
                    if !activated_recovered_queue {
                        append_recovered_active(store, device_id, &session_id, &run_id).await?;
                        touched = true;
                        activated_recovered_queue = true;
                    }
                    recovered.push(RecoveredWork::Retry(AcceptedRunRetry {
                        session_id: session_id.clone(),
                        run_id,
                        failed_run_id,
                        prompt_run_id,
                        user_seq,
                        accepted_seq,
                        worker_generation: store.worker_generation(),
                        backoff_event_id: None,
                    }));
                }
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
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    ),
                    checkpoint,
                    committed_answer,
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
                            accepted_seq,
                            store.worker_generation(),
                            reduction.branch_id.clone(),
                        ),
                        checkpoint,
                        committed_answer,
                    },
                )));
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
                        accepted_seq,
                        store.worker_generation(),
                        reduction.branch_id.clone(),
                    ),
                    checkpoint,
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
            if let Ok(EventPayload::RunState(next)) =
                serde_json::from_value::<EventPayload>(envelope.payload)
            {
                state = Some(next);
            }
        }
    }
}

fn reduce(reductions: &mut HashMap<RunId, RunReduction>, envelope: &RawEnvelope) {
    let Some(run_id) = envelope.run_id.clone() else {
        return;
    };
    let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok();
    let retry_payload = RunRetryEventPayload::from_payload_value(envelope.payload.clone()).ok();
    if payload.is_none() && retry_payload.is_none() {
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
            payload.as_ref(),
            Some(
                EventPayload::RunState(_)
                    | EventPayload::UserMessage { .. }
                    | EventPayload::Item(_)
                    | EventPayload::MenuOpened(_)
                    | EventPayload::MenuAnswered(_)
                    | EventPayload::MenuClosed { .. }
                    | EventPayload::ToolResult { .. }
                    | EventPayload::AgentReport(_)
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
    let Some(payload) = payload else {
        return;
    };
    match payload {
        EventPayload::RunState(state) => {
            reduction.state = Some((state, envelope.seq));
            reduction.state_generation = envelope.worker_generation;
        }
        EventPayload::UserMessage { .. } => reduction.user_seq = Some(envelope.seq),
        EventPayload::Item(ItemEvent::Started { item_id, item }) => {
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
                    text: String::new(),
                    args: String::new(),
                },
            );
        }
        EventPayload::Item(ItemEvent::Delta { item_id, delta }) => {
            if let Some(open) = reduction.open_items.get_mut(&item_id) {
                match &delta {
                    ItemDelta::Text { text } | ItemDelta::Reasoning { text } => {
                        open.text.push_str(text);
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
        EventPayload::Item(ItemEvent::Completed { item_id, item }) => {
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
        EventPayload::ToolResult { call_id, .. } => {
            reduction.tool_results.insert(call_id);
        }
        EventPayload::AgentReport(report) => {
            reduction.agent_reports.insert(report.agent);
        }
        _ => {}
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
    accepted_seq: u64,
    worker_generation: u64,
    branch_id: Option<BranchId>,
) -> AcceptedTurn {
    AcceptedTurn {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
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
    let payloads = interrupted_terminal_payloads(
        reduction,
        cancelling,
        ErrorCode::Internal,
        "run was interrupted by daemon restart".into(),
        true,
    );
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
    let (cancelling, failure_code, failure_message, retryable) = match error {
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
            TurnItem::AgentMessage { .. } => TurnItem::AgentMessage { text: open.text },
            TurnItem::Reasoning { .. } => TurnItem::Reasoning { summary: open.text },
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
                payload: serde_json::to_value(payload).map_err(|error| {
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
#[allow(clippy::expect_used)]
mod streaming_checkpoint_tests {
    use super::*;

    #[derive(Default)]
    struct RecordingVisitor {
        active_session: Option<SessionId>,
        counts: HashMap<SessionId, usize>,
        pages: usize,
    }

    #[async_trait::async_trait]
    impl StartupJournalVisitor for RecordingVisitor {
        async fn start_session(&mut self, session_id: &SessionId) -> Result<u64, HaiderError> {
            assert!(self.active_session.replace(session_id.clone()).is_none());
            Ok(0)
        }

        async fn visit_page(
            &mut self,
            session_id: &SessionId,
            page: &[RawEnvelope],
        ) -> Result<(), HaiderError> {
            assert_eq!(self.active_session.as_ref(), Some(session_id));
            *self.counts.entry(session_id.clone()).or_default() += page.len();
            self.pages += 1;
            Ok(())
        }

        async fn finish_session(
            &mut self,
            _store: &SqliteStoreHandle,
            session_id: &SessionId,
        ) -> Result<(), HaiderError> {
            assert_eq!(self.active_session.take().as_ref(), Some(session_id));
            Ok(())
        }
    }

    fn fact(
        store: &SqliteStoreHandle,
        session_id: &SessionId,
        run_id: &RunId,
        event_id: &str,
        payload: EventPayload,
    ) -> RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(run_id.clone()),
            agent_id: None,
            device_id: DeviceId::new("turn-checkpoint-test"),
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
            payload: serde_json::to_value(payload).expect("encode recovery fact"),
        }
    }

    #[tokio::test]
    async fn turn_recovery_checkpoint_resumes_at_its_journal_high_water() {
        let root = tempfile::tempdir().expect("profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let session_id = SessionId::new("turn-checkpoint-resume");
        let run_id = RunId::new("turn-checkpoint-run");
        let mut prefix = [
            fact(
                &store,
                &session_id,
                &run_id,
                "turn-checkpoint-user",
                EventPayload::UserMessage {
                    text: "hello".into(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Steer,
                },
            ),
            fact(
                &store,
                &session_id,
                &run_id,
                "turn-checkpoint-thinking",
                EventPayload::RunState(RunState::Thinking),
            ),
        ];
        StoreHandle::append(&store, &mut prefix)
            .await
            .expect("append prefix");
        let mut reductions = HashMap::new();
        for envelope in &prefix {
            reduce(&mut reductions, envelope);
        }
        let boundary = prefix.last().expect("prefix boundary");
        put_recovery_checkpoint(
            &store,
            &session_id,
            boundary.seq,
            boundary.event_id.clone(),
            &reductions,
        )
        .await
        .expect("persist checkpoint");
        let mut suffix = [fact(
            &store,
            &session_id,
            &run_id,
            "turn-checkpoint-streaming",
            EventPayload::RunState(RunState::Streaming),
        )];
        StoreHandle::append(&store, &mut suffix)
            .await
            .expect("append suffix");

        let (resumed, cursor) = load_recovery_checkpoint(&store, &session_id)
            .await
            .expect("load checkpoint");
        assert_eq!(cursor, 2, "only the suffix remains to fold");
        let resumed_bytes = rmp_serde::to_vec_named(&DurableTurnRecoveryCheckpointRef {
            shape_version: CHECKPOINT_SHAPE_VERSION,
            reducer_version: CHECKPOINT_REDUCER_VERSION,
            through_seq: cursor,
            reductions: &resumed,
        })
        .expect("encode resumed state");
        let checkpoint = store
            .projection_checkpoint(
                &session_id,
                CHECKPOINT_PROJECTION.to_owned(),
                CHECKPOINT_TIMELINE.to_owned(),
            )
            .await
            .expect("read checkpoint")
            .expect("checkpoint exists");
        assert_eq!(resumed_bytes, checkpoint.payload);

        let recovered =
            recover_interrupted_turns(&store, &DeviceId::new("turn-checkpoint-resume-device"))
                .await
                .expect("fold checkpoint suffix");
        assert!(recovered.is_empty());
        let (resumed, cursor) = load_recovery_checkpoint(&store, &session_id)
            .await
            .expect("load advanced checkpoint");
        assert_eq!(cursor, 3);
        let reduction = resumed.get(&run_id).expect("resumed run reduction");
        assert_eq!(reduction.user_seq, Some(1));
        assert_eq!(
            reduction.state.as_ref().map(|(state, _)| state),
            Some(&RunState::Streaming)
        );
    }

    #[tokio::test]
    async fn corrupt_turn_recovery_checkpoint_falls_back_to_streaming_from_zero() {
        let root = tempfile::tempdir().expect("profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let session_id = SessionId::new("turn-checkpoint-corrupt");
        let run_id = RunId::new("turn-checkpoint-corrupt-run");
        let mut envelope = [fact(
            &store,
            &session_id,
            &run_id,
            "turn-checkpoint-corrupt-fact",
            EventPayload::RunState(RunState::Thinking),
        )];
        StoreHandle::append(&store, &mut envelope)
            .await
            .expect("append journal fact");
        store
            .put_projection_checkpoint(SessionProjectionCheckpoint {
                session_id: session_id.clone(),
                projection: CHECKPOINT_PROJECTION.to_owned(),
                timeline_key: CHECKPOINT_TIMELINE.to_owned(),
                through_seq: envelope[0].seq,
                boundary_event_id: envelope[0].event_id.clone(),
                payload: b"not a turn reducer checkpoint".to_vec(),
            })
            .await
            .expect("install corrupt checkpoint payload");

        let (reductions, cursor) = load_recovery_checkpoint(&store, &session_id)
            .await
            .expect("corrupt checkpoint is a cache miss");
        assert_eq!(cursor, 0);
        assert!(reductions.is_empty());

        recover_interrupted_turns(&store, &DeviceId::new("turn-checkpoint-corrupt-device"))
            .await
            .expect("full streaming fallback repairs checkpoint");
        let (reductions, cursor) = load_recovery_checkpoint(&store, &session_id)
            .await
            .expect("load repaired checkpoint");
        assert_eq!(cursor, 1);
        assert_eq!(
            reductions
                .get(&run_id)
                .and_then(|reduction| reduction.state.as_ref())
                .map(|(state, _)| state),
            Some(&RunState::Thinking)
        );
    }

    #[tokio::test]
    async fn shared_visitor_receives_each_multi_session_page_once() {
        let root = tempfile::tempdir().expect("profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let mut expected = HashMap::new();
        for name in ["shared-stream-alpha", "shared-stream-beta"] {
            let session_id = SessionId::new(name);
            let first_run_id = RunId::new(format!("{name}-run-0"));
            let mut envelopes = (0..=PAGE_SIZE)
                .map(|index| {
                    let run_id = RunId::new(format!("{name}-run-{index}"));
                    fact(
                        &store,
                        &session_id,
                        &run_id,
                        &format!("{name}-{index}"),
                        EventPayload::RunState(RunState::Done),
                    )
                })
                .collect::<Vec<_>>();
            let mut post_terminal_hook = fact(
                &store,
                &session_id,
                &first_run_id,
                &format!("{name}-post-terminal-hook"),
                EventPayload::RunState(RunState::Done),
            );
            post_terminal_hook.payload = haider_protocol::hook::HookEventPayload::HookNotice(
                haider_protocol::hook::HookNotice {
                    hook: Some("post-terminal".into()),
                    digest: None,
                    source: "startup-test".into(),
                    reason: "valid asynchronous suffix".into(),
                },
            )
            .to_payload_value()
            .expect("encode hook suffix");
            envelopes.push(post_terminal_hook);
            let selected_envelopes = envelopes.len() - 1;
            StoreHandle::append(&store, &mut envelopes)
                .await
                .expect("append page-crossing recovery journal");
            expected.insert(session_id, selected_envelopes);
        }

        let mut visitor = RecordingVisitor::default();
        let recovery = recover_interrupted_turns_report_with_visitor(
            &store,
            &DeviceId::new("shared-stream-device"),
            &mut visitor,
        )
        .await
        .expect("shared startup scan");
        assert!(recovery.work.is_empty());
        assert_eq!(visitor.counts, expected);
        assert!(
            visitor.pages >= 4,
            "each session must cross a page boundary"
        );
        assert!(visitor.active_session.is_none());
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
