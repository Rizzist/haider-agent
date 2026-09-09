//! Durable follow-up lifecycle. The source journal is the observation receipt;
//! admission, terminal state and explicit handling remain separate facts.

use crate::monitor::{MonitorDeliveryReceipt, MonitorError, MonitorReport};
use crate::session_hub::{HubStoreHandle, SessionHub};
use haider_core::StoreHandle;
use haider_protocol::completion::{
    CompletionEvent, CompletionObligation, CompletionParkReason, CompletionProjection,
    CompletionSource, CompletionStatus, MAX_COMPLETION_ATTEMPTS,
};
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{EventId, RunId, SessionId};
use haider_protocol::{
    EventPayload,
    error::{ErrorCode, HaiderError},
};
use haider_tools::CompletionControl;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, serde::Serialize)]
pub(crate) struct ActionEvidence {
    pub event_id: EventId,
    pub seq: u64,
    pub run_id: Option<RunId>,
    pub call_id: Option<String>,
    pub report: bool,
}

impl ActionEvidence {
    fn new(envelope: &RawEnvelope, call_id: Option<String>) -> Self {
        Self {
            event_id: envelope.event_id.clone(),
            seq: envelope.seq,
            run_id: envelope.run_id.clone(),
            report: call_id.is_none(),
            call_id,
        }
    }
}

#[derive(Default, Clone)]
pub(crate) struct CompletionJournal {
    cursor: u64,
    pub projection: CompletionProjection,
    pub reports: BTreeMap<String, MonitorReport>,
    pub evidence: Vec<ActionEvidence>,
    tools: HashMap<(Option<RunId>, String), String>,
    pub events: Vec<CompletionEvent>,
}

pub(crate) async fn load(
    hub: &SessionHub,
    session: &SessionId,
) -> Result<CompletionJournal, HaiderError> {
    let mut cache = hub.inner_monitor().completion_cache().await;
    if cache.len() >= 32 && !cache.contains_key(session) {
        cache.clear();
    }
    let journal = cache.entry(session.clone()).or_default();
    loop {
        let page = hub
            .read_internal_session(session, journal.cursor, 256)
            .await?;
        if page.is_empty() {
            break;
        }
        for envelope in page {
            journal.cursor = envelope.seq;
            if envelope.payload.get("type").and_then(|v| v.as_str())
                == Some("monitor_report_pending")
                && let Some(value) = envelope.payload.pointer("/pending/report")
                && let Ok(report) = serde_json::from_value::<MonitorReport>(value.clone())
                && report.session_id == *session
            {
                journal
                    .reports
                    .insert(format!("monitor:{}", report.report_id), report);
            }
            let kind = envelope
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if kind.starts_with("completion_")
                && let Ok(event) = envelope.payload.decode::<CompletionEvent>()
            {
                journal.events.push(event);
            }
            if kind == "tool_result"
                || (kind == "item"
                    && matches!(
                        envelope.payload.get("event").and_then(|v| v.as_str()),
                        Some("started" | "completed")
                    ))
            {
                match envelope.payload.decode_event() {
                    Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Started {
                        item: haider_protocol::item::TurnItem::ToolCall { call_id, name, .. },
                        ..
                    })) => {
                        journal
                            .tools
                            .insert((envelope.run_id.clone(), call_id), name);
                    }
                    Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                        item: haider_protocol::item::TurnItem::AgentMessage { ref text },
                        ..
                    })) if !text.is_empty() => {
                        journal.evidence.push(ActionEvidence::new(&envelope, None));
                    }
                    Ok(EventPayload::ToolResult { call_id, result })
                        if journal
                            .tools
                            .remove(&(envelope.run_id.clone(), call_id.clone()))
                            .is_some_and(|name| {
                                !matches!(name.as_str(), "monitor" | "list_tools" | "request_input")
                            })
                            && result.status.is_completed() =>
                    {
                        journal
                            .evidence
                            .push(ActionEvidence::new(&envelope, Some(call_id)));
                    }
                    _ => {}
                }
            }
            journal.projection.apply(&envelope);
        }
    }
    journal
        .reports
        .retain(|id, _| journal.projection.pending.contains_key(id));
    let oldest = journal
        .projection
        .pending
        .values()
        .map(|o| o.created_seq)
        .min()
        .unwrap_or(u64::MAX);
    journal.evidence.retain(|e| e.seq > oldest);
    // At most the recent successful results are advertised; older results
    // remain in the durable journal and may be reconciled by a fresh tool.
    if journal.evidence.len() > 128 {
        journal.evidence.drain(..journal.evidence.len() - 128);
    }
    Ok(journal.clone())
}

fn invalid(message: &str) -> HaiderError {
    HaiderError::new(ErrorCode::InvalidArgument, message, false)
}

pub(crate) fn envelope(
    hub: &SessionHub,
    obligation: &CompletionObligation,
    event: &CompletionEvent,
) -> Result<RawEnvelope, HaiderError> {
    let payload =
        serde_json::to_value(event).map_err(|_| invalid("cannot encode follow-up transition"))?;
    let mut identity = blake3::Hasher::new();
    identity.update(obligation.session_id.as_str().as_bytes());
    identity.update(&[0]);
    identity.update(payload.to_string().as_bytes());
    let digest = identity.finalize().to_hex();
    Ok(EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(format!("completion-{digest}")),
        seq: 0,
        session_id: obligation.session_id.clone(),
        branch_id: obligation.branch_id.clone(),
        run_id: None,
        agent_id: obligation.agent_id.clone(),
        device_id: hub.device_id(),
        authority_epoch: 0,
        worker_generation: hub.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: payload.into(),
    })
}

pub(crate) async fn append(
    hub: &SessionHub,
    obligation: &CompletionObligation,
    event: CompletionEvent,
) -> Result<(), HaiderError> {
    hub.append(&mut [envelope(hub, obligation, &event)?])
        .await?;
    Ok(())
}

pub(crate) async fn wake_monitor(
    hub: &SessionHub,
    report: MonitorReport,
) -> Result<MonitorDeliveryReceipt, MonitorError> {
    let service = hub.inner_monitor();
    let _guard = service.completion_lock().await;
    if service.completion_retired(&report.session_id) {
        return Ok(retained_receipt());
    }
    let journal = load(hub, &report.session_id).await.map_err(store_error)?;
    let id = format!("monitor:{}", report.report_id);
    let Some(obligation) = journal.projection.pending.get(&id) else {
        // Direct transport seam callers predate the outbox. Settled reports
        // must never reach this fallback; their durable source proves ownership.
        if journal.projection.is_settled(&id) {
            return Ok(retained_receipt());
        }
        return hub.admit_monitor_report(report, None).await;
    };
    if !matches!(
        obligation.status,
        CompletionStatus::Pending | CompletionStatus::Admitting
    ) {
        return Ok(retained_receipt());
    }
    let attempt = if obligation.status == CompletionStatus::Admitting {
        obligation.attempt
    } else {
        if obligation.attempt >= MAX_COMPLETION_ATTEMPTS {
            append(
                hub,
                obligation,
                CompletionEvent::CompletionParked {
                    obligation_id: id,
                    attempt: obligation.attempt,
                    reason: CompletionParkReason::AttemptsExhausted,
                },
            )
            .await
            .map_err(store_error)?;
            return Ok(retained_receipt());
        }
        let attempt = obligation.attempt + 1;
        append(
            hub,
            obligation,
            CompletionEvent::CompletionAttempt {
                obligation_id: id,
                attempt,
                worker_generation: hub.worker_generation(),
            },
        )
        .await
        .map_err(store_error)?;
        attempt
    };
    hub.admit_monitor_report(report, Some(attempt)).await
}

fn retained_receipt() -> MonitorDeliveryReceipt {
    MonitorDeliveryReceipt {
        durable: true,
        handed_off: false,
        disposition: "follow_up_pending",
    }
}

fn store_error(error: HaiderError) -> MonitorError {
    MonitorError::Store(error.message)
}

pub(crate) async fn admitted(
    hub: &SessionHub,
    report: &MonitorReport,
    attempt: u32,
    run_id: &RunId,
    accepted_seq: u64,
    generation: u64,
) -> Result<(), MonitorError> {
    let journal = load(hub, &report.session_id).await.map_err(store_error)?;
    let id = format!("monitor:{}", report.report_id);
    if let Some(obligation) = journal.projection.pending.get(&id) {
        let event = CompletionEvent::CompletionAdmitted {
            obligation_id: id,
            attempt,
            run_id: run_id.clone(),
            accepted_seq,
            worker_generation: generation,
        };
        if !journal.events.contains(&event) {
            append(hub, obligation, event).await.map_err(store_error)?;
        }
    }
    Ok(())
}

pub(crate) async fn control(
    hub: &SessionHub,
    store: &HubStoreHandle,
    coordinates: &crate::monitor::MonitorToolCoordinates,
    id: &str,
    action: CompletionControl,
    attempt: u32,
    evidence_ids: Vec<EventId>,
) -> Result<serde_json::Value, HaiderError> {
    let run_id = &coordinates.run_id;
    let service = hub.inner_monitor();
    let _guard = service.completion_lock().await;
    let journal = load(hub, store.session_id()).await?;
    let event = match action {
        CompletionControl::Handled => CompletionEvent::CompletionHandled {
            obligation_id: id.into(),
            attempt,
            run_id: run_id.clone(),
            evidence_event_ids: evidence_ids.clone(),
        },
        CompletionControl::Dismiss => CompletionEvent::CompletionDismissed {
            obligation_id: id.into(),
            attempt,
        },
        CompletionControl::Resume => CompletionEvent::CompletionResumed {
            obligation_id: id.into(),
            attempt,
        },
        CompletionControl::Claim => CompletionEvent::CompletionAttempt {
            obligation_id: id.into(),
            attempt: attempt.saturating_add(1),
            worker_generation: hub.worker_generation(),
        },
    };
    if action != CompletionControl::Claim && journal.events.contains(&event) {
        return Ok(serde_json::json!({"status":"replayed", "receipt": event}));
    }
    let obligation = journal
        .projection
        .pending
        .get(id)
        .ok_or_else(|| invalid("unknown or settled follow-up obligation"))?;
    if obligation.branch_id != coordinates.branch_id || obligation.agent_id != coordinates.agent_id
    {
        return Err(invalid("follow-up belongs to a different branch or agent"));
    }
    if obligation.attempt != attempt {
        return Err(invalid("stale follow-up attempt; list current obligations"));
    }
    let mut envelopes = Vec::new();
    match action {
        CompletionControl::Handled => {
            if obligation.run_id.as_ref() != Some(run_id)
                || evidence_ids.is_empty()
                || evidence_ids.iter().any(|id| {
                    !journal.evidence.iter().any(|e| {
                        e.event_id == *id
                            && e.run_id.as_ref() == Some(run_id)
                            && e.seq > obligation.accepted_seq.unwrap_or(obligation.created_seq)
                            && (obligation.report_only || !e.report)
                    })
                })
            {
                return Err(invalid(
                    "handled requires the consuming run and committed action-result evidence (or a completed owner report for a report-only monitor) after this attempt was admitted; reconcile effects before claiming success",
                ));
            }
        }
        CompletionControl::Claim => {
            if matches!(
                obligation.status,
                CompletionStatus::Active | CompletionStatus::Admitting
            ) {
                if obligation.run_id.as_ref() == Some(run_id) {
                    return Ok(
                        serde_json::json!({"status":"already_claimed", "attempt": obligation.attempt}),
                    );
                }
                return Err(invalid("another consuming run already owns this follow-up"));
            }
            if obligation.attempt >= MAX_COMPLETION_ATTEMPTS {
                return Err(invalid(
                    "follow-up attempt budget exhausted; dismiss or resolve manually",
                ));
            }
            envelopes.push(envelope(hub, obligation, &event)?);
            let admitted = CompletionEvent::CompletionAdmitted {
                obligation_id: id.into(),
                attempt: attempt + 1,
                run_id: run_id.clone(),
                accepted_seq: journal.cursor,
                worker_generation: hub.worker_generation(),
            };
            envelopes.push(envelope(hub, obligation, &admitted)?);
            envelopes.push(envelope(
                hub,
                obligation,
                &CompletionEvent::CompletionDelivered {
                    obligation_id: id.into(),
                    attempt: attempt + 1,
                },
            )?);
        }
        CompletionControl::Resume => {
            if obligation.source != CompletionSource::Monitor {
                return Err(invalid(
                    "task/menu follow-ups never autostart; claim them in the current run",
                ));
            }
            if !matches!(
                obligation.status,
                CompletionStatus::Parked | CompletionStatus::AwaitingReceipt
            ) {
                return Err(invalid(
                    "only parked or unhandled terminal follow-ups can resume",
                ));
            }
            if obligation.attempt >= MAX_COMPLETION_ATTEMPTS {
                return Err(invalid("follow-up attempt budget exhausted"));
            }
        }
        CompletionControl::Dismiss => {}
    }
    if envelopes.is_empty() {
        envelopes.push(envelope(hub, obligation, &event)?);
    }
    store.append(&mut envelopes).await?;
    Ok(serde_json::json!({"status":"accepted", "receipt": event}))
}

pub(crate) async fn prompt(
    hub: &SessionHub,
    session: &SessionId,
    branch: Option<&haider_protocol::ids::BranchId>,
    agent: Option<&haider_protocol::ids::AgentId>,
) -> Result<String, HaiderError> {
    let journal = load(hub, session).await?;
    if journal.projection.pending.is_empty() {
        return Ok(String::new());
    }
    let obligations = journal
        .projection
        .pending
        .values()
        .filter(|o| o.branch_id.as_ref() == branch && o.agent_id.as_ref() == agent)
        .take(16)
        .collect::<Vec<_>>();
    if obligations.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "Pending follow-up obligations (up to 16; monitor list shows the full set; delivery is not handling): {}\nUse monitor list for committed evidence IDs. Claim task/menu work explicitly with monitor follow_up. A handled receipt must reference successful action/reconciliation results. Before retrying side effects, inspect their existing outcome using the stable obligation ID. Do not replay source processes or answered menus.",
        serde_json::to_string(&obligations)
            .map_err(|_| invalid("cannot encode pending follow-ups"))?
    ))
}

/// Uses committed-session publications; no provider polling or new scheduler.
/// Lag and restart rebuild from the same source journal as session.observe.
pub(crate) fn spawn_reconciler(
    weak: crate::session_hub::WeakSessionHub,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(hub) = weak.upgrade() else {
            return;
        };
        let mut changes = hub.subscribe_completion_reconcile();
        let initial = hub.session_ids().await;
        let mut rescan = initial.is_err();
        let mut sessions = initial.unwrap_or_else(|error| {
            tracing::warn!(%error, "follow-up startup scan will retry");
            Vec::new()
        });
        drop(hub);
        let mut retries = Vec::new();
        loop {
            if *shutdown.borrow() {
                return;
            }
            for session in sessions.drain(..) {
                let Some(hub) = weak.upgrade() else {
                    return;
                };
                let result = tokio::select! {
                    result = reconcile(&hub, &session) => result,
                    _ = shutdown.changed() => return,
                };
                if let Err(error) = result {
                    tracing::warn!(%session, %error, "follow-up reconciliation remains pending");
                    retries.push(session);
                }
            }
            tracing::debug!(
                retry_count = retries.len(),
                "follow-up reconciliation pass finished"
            );
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(5)), if rescan || !retries.is_empty() => {
                    if rescan {
                        let Some(hub) = weak.upgrade() else { return; };
                        match hub.session_ids().await { Ok(found) => { sessions.extend(found); rescan = false; }, Err(error) => tracing::warn!(%error, "follow-up session scan will retry") }
                    }
                    sessions.append(&mut retries); sessions.sort_by(|a,b| a.as_str().cmp(b.as_str())); sessions.dedup();
                },
                change = changes.recv() => match change {
                    Ok(session) => sessions.push(session),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let Some(hub) = weak.upgrade() else { return; };
                        match hub.session_ids().await { Ok(found) => sessions = found, Err(error) => { tracing::warn!(%error, "follow-up lag recovery scan will retry"); rescan = true; } }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
                _ = shutdown.changed() => return,
            }
        }
    })
}

pub(crate) async fn reconcile(hub: &SessionHub, session: &SessionId) -> Result<(), MonitorError> {
    let service = hub.inner_monitor();
    if service.completion_retired(session) {
        return Ok(());
    }
    let guard = service.completion_lock().await;
    if service.completion_retired(session) {
        return Ok(());
    }
    let journal = load(hub, session).await.map_err(store_error)?;
    let mut wake = Vec::new();
    for obligation in journal.projection.pending.values() {
        let restart_unhandled = obligation.status == CompletionStatus::AwaitingReceipt
            && obligation.worker_generation != hub.worker_generation();
        if let Some(reason) = obligation
            .park_reason
            .or(restart_unhandled.then_some(CompletionParkReason::Interrupted))
        {
            let parked = CompletionEvent::CompletionParked {
                obligation_id: obligation.obligation_id.clone(),
                attempt: obligation.attempt,
                reason,
            };
            if !journal.events.contains(&parked) {
                append(hub, obligation, parked).await.map_err(store_error)?;
            }
            if reason == CompletionParkReason::Interrupted
                && obligation.source == CompletionSource::Monitor
                && obligation.attempt < MAX_COMPLETION_ATTEMPTS
            {
                append(
                    hub,
                    obligation,
                    CompletionEvent::CompletionResumed {
                        obligation_id: obligation.obligation_id.clone(),
                        attempt: obligation.attempt,
                    },
                )
                .await
                .map_err(store_error)?;
                if let Some(report) = journal.reports.get(&obligation.obligation_id) {
                    wake.push(report.clone());
                }
            }
        } else if obligation.source == CompletionSource::Monitor
            // The source outbox owns first delivery and its ordered/coalesced
            // follow-up slot, including recovery before admission. Reconciliation
            // only resumes an existing attempt; it cannot bypass that ordering
            // or automatically replay legacy delivered reports without receipts.
            && obligation.attempt > 0
            && matches!(
                obligation.status,
                CompletionStatus::Pending | CompletionStatus::Admitting
            )
            && let Some(report) = journal.reports.get(&obligation.obligation_id)
        {
            wake.push(report.clone());
        }
    }
    drop(guard);
    for report in wake {
        wake_monitor(hub, report).await?;
    }
    Ok(())
}

/// Streaming text and tool deltas must not schedule journal scans.
pub(crate) fn needs_reconcile(envelope: &RawEnvelope) -> bool {
    let kind = envelope
        .payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    kind.starts_with("completion_")
        || matches!(
            kind,
            "monitor_report_pending"
                | "monitor_report_delivered"
                | "monitor_removed"
                | "task_completed"
                | "menu_answered"
                | "run_failed"
                | "run_retried"
        )
        || (kind == "run_state"
            && matches!(envelope.payload.decode_event(), Ok(EventPayload::RunState(state)) if state.is_terminal()))
}
