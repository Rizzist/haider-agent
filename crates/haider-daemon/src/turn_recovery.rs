//! Durable interrupted-run reduction performed before Ready.

use haider_core::{
    AcceptedTurn, RequestInputCheckpoint, SqliteStoreHandle, StoreHandle, TurnAdmissionDisposition,
};
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EventId, ItemId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason};
use haider_protocol::state::{RunState, SessionState};
use std::collections::HashMap;

const PAGE_SIZE: usize = 512;

pub(crate) enum RecoveredWork {
    Queued(AcceptedTurn),
    Checkpoint(Box<RecoveredCheckpoint>),
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
    tool_results: HashMap<String, ()>,
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
                && matches!(&state, RunState::InputRequired { menu } if *menu == checkpoint.menu.id)
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
                match delta {
                    ItemDelta::Text { text } | ItemDelta::Reasoning { text } => {
                        open.text.push_str(&text);
                    }
                    ItemDelta::ToolArgs { fragment } => open.args.push_str(&fragment),
                    ItemDelta::CommandOutput { .. } => {}
                }
            }
        }
        EventPayload::Item(ItemEvent::Completed { item_id, .. }) => {
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
            reduction.tool_results.insert(call_id, ());
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
                if name == "request_input" && !reduction.tool_results.contains_key(call_id) =>
            {
                Some(RequestInputCheckpoint {
                    menu: open_menu.menu.clone(),
                    request_seq: open_menu.request_seq,
                    opening_generation: open_menu.opening_generation,
                    tool_item_id: item_id.clone(),
                    call_id: call_id.clone(),
                    args: open.args.clone(),
                })
            }
            _ => None,
        })
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
            code: ErrorCode::Internal,
            message: "run was interrupted by daemon restart".into(),
            retryable: true,
        });
    }
    payloads.push(EventPayload::RunState(terminal));
    payloads.push(EventPayload::SessionState(SessionState::Idle {
        interrupted: true,
    }));
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
