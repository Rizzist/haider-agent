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
//! - a run parked at a durable tool-menu checkpoint (an open `request_input`
//!   or broker-approved mutating tool without its `ToolResult`, matching
//!   historical `InputRequired` or canonical `PermissionRequired` state and
//!   open menu) is reconstructed as a waiter — its
//!   menu stays PENDING, and neither the provider request that produced it
//!   nor any dispatched effect is ever repeated;
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
    AcceptedTurn, ChildWaitCheckpoint, DeferredTicket, DeferredToolCheckpoint,
    RequestInputCheckpoint, SqliteStoreHandle, StoreHandle, TurnAdmissionDisposition,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason, MenuKind};
use haider_protocol::state::{RunState, SessionState, WaitReason};
use std::collections::{HashMap, HashSet};

const PAGE_SIZE: usize = 512;

pub(crate) enum RecoveredWork {
    Queued(AcceptedTurn),
    Checkpoint(Box<RecoveredCheckpoint>),
    ChildWait(Box<RecoveredChildWait>),
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

#[derive(Default)]
struct RunReduction {
    state: Option<(RunState, u64)>,
    state_generation: u64,
    user_seq: Option<u64>,
    open_items: HashMap<ItemId, OpenItem>,
    menu: Option<OpenMenu>,
    menu_answers: HashMap<MenuId, RawEnvelope>,
    tool_results: HashSet<String>,
    tool_calls: HashMap<String, RecoveredToolCall>,
    agent_reports: HashSet<AgentId>,
    child_results: HashSet<AgentId>,
}

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
    text: String,
    args: String,
}

#[derive(Clone)]
struct OpenMenu {
    menu: Menu,
    request_seq: u64,
    opening_generation: u64,
}

pub(crate) async fn recover_interrupted_turns(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
) -> Result<Vec<RecoveredWork>, HaiderError> {
    let mut recovered = Vec::new();
    for session_id in store.session_ids().await? {
        let runnable_metadata = store.session_metadata(&session_id).await?.is_some();
        let mut cursor = 0;
        let mut reductions = HashMap::<RunId, RunReduction>::new();
        loop {
            let page = store.read(&session_id, cursor, PAGE_SIZE).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            for envelope in page {
                reduce(&mut reductions, envelope);
            }
        }
        let mut runs = reductions.into_iter().collect::<Vec<_>>();
        runs.sort_by_key(|(_, reduction)| reduction.user_seq.unwrap_or(u64::MAX));
        let mut activated_recovered_queue = false;
        for (run_id, reduction) in runs {
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
                    reduction,
                    matches!(state, RunState::Cancelling),
                )
                .await?;
                continue;
            }
            if state == RunState::Queued {
                if let Some(accepted_seq) = reduction.user_seq {
                    if !activated_recovered_queue {
                        append_recovered_active(store, device_id, &session_id, &run_id).await?;
                        activated_recovered_queue = true;
                    }
                    recovered.push(RecoveredWork::Queued(recovered_acceptance(
                        &session_id,
                        &run_id,
                        accepted_seq,
                        store.worker_generation(),
                    )));
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
                    ),
                    checkpoint,
                    committed_answer,
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
                        accepted_seq,
                        store.worker_generation(),
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
                reduction,
                matches!(state, RunState::Cancelling),
            )
            .await?;
        }
    }
    Ok(recovered)
}

fn checkpoint_state_matches(state: &RunState, checkpoint: &RequestInputCheckpoint) -> bool {
    match state {
        // Dual-read migration: old broker approvals and actor-owned input
        // checkpoints both used InputRequired.
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

async fn latest_run_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    run_id: &RunId,
) -> Result<Option<RunState>, HaiderError> {
    let mut cursor = 0;
    let mut state = None;
    loop {
        let page = store.read(session_id, cursor, PAGE_SIZE).await?;
        if page.is_empty() {
            return Ok(state);
        }
        cursor = page.last().map_or(cursor, |envelope| envelope.seq);
        for envelope in page {
            if envelope.run_id.as_ref() != Some(run_id) {
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

fn reduce(reductions: &mut HashMap<RunId, RunReduction>, envelope: RawEnvelope) {
    let Some(run_id) = envelope.run_id.clone() else {
        return;
    };
    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
        return;
    };
    let reduction = reductions.entry(run_id).or_default();
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
            reduction.menu_answers.insert(answer.menu, envelope);
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
                            name != "request_input"
                        }
                        _ => name == "request_input",
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
) -> AcceptedTurn {
    AcceptedTurn {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        accepted_seq,
        worker_generation,
        disposition: TurnAdmissionDisposition::Started,
    }
}

async fn terminalize_interrupted(
    store: &SqliteStoreHandle,
    device_id: &DeviceId,
    session_id: &SessionId,
    run_id: &RunId,
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
        for envelope in page {
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
                branch_id: None,
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
