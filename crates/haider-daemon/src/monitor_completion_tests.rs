#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::monitor_tests::{MonitorWorld, registration, test_report};
use super::*;
use haider_core::SqliteStoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::completion::{CompletionSource, CompletionStatus};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::RunState;
use haider_tools::CompletionControl;

async fn pending_task(world: &MonitorWorld) -> String {
    let task = haider_protocol::task::TaskCompleted {
        completion_consumer: None,
        task: haider_protocol::ids::TaskId::new("completion-task"),
        name: "harmless".into(),
        state: haider_protocol::task::TaskTerminalState::Completed { exit_code: Some(0) },
        elapsed_ms: 1,
        output_bytes: 0,
        output_sha256: None,
        tail: String::new(),
        output_digest: None,
        output_digest_complete: false,
        artifact: None,
        full_output_unavailable: false,
        truncated: false,
        delivery: haider_protocol::task::TaskCompletionDelivery::DeliveredQueued,
        workspace_mutation: None,
    };
    world
        .hub
        .append(&mut [monitor_envelope(
            &world.session,
            Some(&world.run),
            None,
            None,
            "completion-task-result",
            world.hub.device_id(),
            world.hub.worker_generation(),
            task.to_payload_value().unwrap(),
        )])
        .await
        .unwrap();
    "task:completion-task".into()
}

async fn action_result(world: &MonitorWorld, name: &str) -> EventId {
    let item_id = haider_protocol::ids::ItemId::new(format!("action-{name}"));
    let call_id = format!("action-call-{name}");
    let item = TurnItem::ToolCall {
        call_id: call_id.clone(),
        name: name.into(),
        args: json!({}),
        status: ToolStatus::InProgress,
        activity: None,
    };
    let result = tool_result(
        json!({"reconciled":true}),
        ToolResultStatus::Completed,
        None,
    );
    let payloads = [
        EventPayload::Item(ItemEvent::Started {
            item_id: item_id.clone(),
            item,
        }),
        EventPayload::ToolResult {
            call_id: call_id.clone(),
            result,
        },
        EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::ToolCall {
                call_id,
                name: name.into(),
                args: json!({}),
                status: ToolStatus::Completed,
                activity: None,
            },
        }),
    ];
    let mut envelopes = payloads
        .into_iter()
        .enumerate()
        .map(|(n, p)| {
            monitor_envelope(
                &world.session,
                Some(&world.run),
                None,
                None,
                &format!("evidence-{name}-{n}"),
                world.hub.device_id(),
                world.hub.worker_generation(),
                serde_json::to_value(p).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    world.lease.append(&mut envelopes).await.unwrap();
    envelopes[1].event_id.clone()
}

#[tokio::test]
async fn durable_task_claim_requires_action_evidence_and_receipt_replay_is_idempotent() {
    let world = MonitorWorld::new("completion-receipt").await;
    let id = pending_task(&world).await;
    let pending = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(
        pending.projection.pending[&id].source,
        CompletionSource::Task
    );
    assert_eq!(
        pending.projection.pending[&id].status,
        CompletionStatus::Pending
    );
    world
        .execute(
            "claim",
            MonitorRequest::FollowUp {
                obligation_id: id.clone(),
                action: CompletionControl::Claim,
                attempt: 0,
                evidence_event_ids: vec![],
            },
        )
        .await;
    let wrong = world
        .hub
        .execute_monitor_tool(
            &world.lease,
            world.coordinates("bad-ack"),
            MonitorRequest::FollowUp {
                obligation_id: id.clone(),
                action: CompletionControl::Handled,
                attempt: 1,
                evidence_event_ids: vec![EventId::new("invented-effect")],
            },
        )
        .await;
    assert!(wrong.is_err());
    let monitor_result = action_result(&world, "monitor").await;
    assert!(
        world
            .hub
            .execute_monitor_tool(
                &world.lease,
                world.coordinates("self-ack"),
                MonitorRequest::FollowUp {
                    obligation_id: id.clone(),
                    action: CompletionControl::Handled,
                    attempt: 1,
                    evidence_event_ids: vec![monitor_result]
                }
            )
            .await
            .is_err(),
        "listing or claiming is not action evidence"
    );
    let evidence = action_result(&world, "fs_read").await;
    let request = MonitorRequest::FollowUp {
        obligation_id: id.clone(),
        action: CompletionControl::Handled,
        attempt: 1,
        evidence_event_ids: vec![evidence],
    };
    world.execute("ack", request.clone()).await;
    let replay = world.execute("duplicate-ack", request).await;
    assert!(replay.preview.contains("replayed"));
    world.hub.inner_monitor().completion_cache().await.clear();
    assert!(
        !crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap()
            .projection
            .pending
            .contains_key(&id),
        "cold journal replay preserves receipt"
    );
}

#[tokio::test]
async fn task_resume_does_not_autostart_and_dismissal_survives_cold_projection() {
    let world = MonitorWorld::new("completion-task-policy").await;
    let id = pending_task(&world).await;
    assert!(
        world
            .hub
            .execute_monitor_tool(
                &world.lease,
                world.coordinates("resume"),
                MonitorRequest::FollowUp {
                    obligation_id: id.clone(),
                    action: CompletionControl::Resume,
                    attempt: 0,
                    evidence_event_ids: vec![]
                }
            )
            .await
            .is_err()
    );
    world
        .execute(
            "dismiss",
            MonitorRequest::FollowUp {
                obligation_id: id.clone(),
                action: CompletionControl::Dismiss,
                attempt: 0,
                evidence_event_ids: vec![],
            },
        )
        .await;
    world.hub.inner_monitor().completion_cache().await.clear();
    assert!(
        crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap()
            .projection
            .is_settled(&id)
    );
}

#[tokio::test]
async fn a_new_attempt_requires_new_action_or_reconciliation_evidence() {
    let world = MonitorWorld::new("completion-reconcile").await;
    let id = pending_task(&world).await;
    let earlier = action_result(&world, "fs_read").await;
    world
        .execute(
            "claim",
            MonitorRequest::FollowUp {
                obligation_id: id.clone(),
                action: CompletionControl::Claim,
                attempt: 0,
                evidence_event_ids: vec![],
            },
        )
        .await;
    assert!(
        world
            .hub
            .execute_monitor_tool(
                &world.lease,
                world.coordinates("old-result"),
                MonitorRequest::FollowUp {
                    obligation_id: id.clone(),
                    action: CompletionControl::Handled,
                    attempt: 1,
                    evidence_event_ids: vec![earlier]
                }
            )
            .await
            .is_err(),
        "earlier work alone cannot prove the new attempt reconciled the action"
    );
    let evidence = action_result(&world, "fs_write").await;
    let mut wrong_branch = world.coordinates("wrong-branch");
    wrong_branch.branch_id = Some(BranchId::new("unrelated-branch"));
    let receipt = MonitorRequest::FollowUp {
        obligation_id: id,
        action: CompletionControl::Handled,
        attempt: 1,
        evidence_event_ids: vec![evidence],
    };
    assert!(
        world
            .hub
            .execute_monitor_tool(&world.lease, wrong_branch, receipt.clone())
            .await
            .is_err()
    );
    world.execute("correct-receipt", receipt).await;
}

#[tokio::test]
async fn follow_up_attempt_budget_survives_cache_eviction() {
    use haider_protocol::completion::{
        CompletionEvent, CompletionParkReason, MAX_COMPLETION_ATTEMPTS,
    };
    let world = MonitorWorld::new("completion-attempt-budget").await;
    let id = pending_task(&world).await;
    for attempt in 0..MAX_COMPLETION_ATTEMPTS {
        world
            .execute(
                &format!("claim-{attempt}"),
                MonitorRequest::FollowUp {
                    obligation_id: id.clone(),
                    action: CompletionControl::Claim,
                    attempt,
                    evidence_event_ids: vec![],
                },
            )
            .await;
        let journal = crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap();
        crate::completion::append(
            &world.hub,
            &journal.projection.pending[&id],
            CompletionEvent::CompletionParked {
                obligation_id: id.clone(),
                attempt: attempt + 1,
                reason: CompletionParkReason::ProviderRecovery,
            },
        )
        .await
        .unwrap();
        world.hub.inner_monitor().completion_cache().await.clear();
    }
    assert!(
        world
            .hub
            .execute_monitor_tool(
                &world.lease,
                world.coordinates("over-budget"),
                MonitorRequest::FollowUp {
                    obligation_id: id.clone(),
                    action: CompletionControl::Claim,
                    attempt: MAX_COMPLETION_ATTEMPTS,
                    evidence_event_ids: vec![]
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap()
            .projection
            .pending[&id]
            .status,
        CompletionStatus::Parked
    );
}

async fn observe_monitor(world: &MonitorWorld, report: MonitorReport) {
    let pending = PendingMonitorReport {
        report: report.clone(),
        terminal_reason: (report.occurrence == MonitorOccurrence::Once)
            .then_some(MonitorRemovalReason::OneShotComplete),
        queue_order: 1,
        queued_at_ms: 1,
    };
    let payload =
        serde_json::to_value(MonitorJournalEvent::MonitorReportPending { pending }).unwrap();
    world
        .hub
        .append(&mut [monitor_envelope(
            &world.session,
            None,
            None,
            None,
            &format!("observed-{}", report.report_id),
            world.hub.device_id(),
            world.hub.worker_generation(),
            payload,
        )])
        .await
        .unwrap();
}

/// Both durable obligation states are dismissed while a stop owns the
/// session. A later replay must stay settled after the guard drops.
#[tokio::test]
async fn fenced_monitor_wake_dismisses_pending_and_admitting_obligations() {
    for admitting in [false, true] {
        assert_fenced_monitor_obligation_dismissal(admitting).await;
    }
}

#[tokio::test]
async fn fenced_monitor_wake_dismisses_pending_obligation() {
    assert_fenced_monitor_obligation_dismissal(false).await;
}

#[tokio::test]
async fn fenced_monitor_wake_dismisses_admitting_obligation() {
    assert_fenced_monitor_obligation_dismissal(true).await;
}

async fn assert_fenced_monitor_obligation_dismissal(admitting: bool) {
    use haider_protocol::completion::CompletionEvent;
    let label = if admitting {
        "fenced-admitting"
    } else {
        "fenced-pending"
    };
    let world = MonitorWorld::new(label).await;
    let mut report = test_report(label, "stop wake", MonitorReportStatus::Matched);
    report.session_id = world.session.clone();
    observe_monitor(&world, report.clone()).await;
    let id = format!("monitor:{label}");
    if admitting {
        let journal = crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap();
        crate::completion::append(
            &world.hub,
            &journal.projection.pending[&id],
            CompletionEvent::CompletionAttempt {
                obligation_id: id.clone(),
                attempt: 1,
                worker_generation: world.hub.worker_generation(),
            },
        )
        .await
        .unwrap();
    }
    let before = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(
        before.projection.pending[&id].status,
        if admitting {
            CompletionStatus::Admitting
        } else {
            CompletionStatus::Pending
        }
    );
    let fence = world.hub.inner_monitor().stop_fences().fence(
        &world.session,
        &[],
        std::slice::from_ref(&report.monitor_id),
    );
    let receipt = world.hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(receipt.durable);
    assert!(!receipt.handed_off);
    assert_eq!(receipt.disposition, "follow_up_pending");
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert!(journal.projection.is_settled(&id));
    assert_eq!(
        journal
            .events
            .iter()
            .filter(|event| matches!(event,
                CompletionEvent::CompletionDismissed { obligation_id, .. } if obligation_id == &id
            ))
            .count(),
        1
    );
    assert!(
        !journal.events.iter().any(|event| matches!(event,
            CompletionEvent::CompletionAdmitted { obligation_id, .. } if obligation_id == &id
        )),
        "dismissal must not admit a monitor run"
    );
    drop(fence);
    world.hub.inner_monitor().completion_cache().await.clear();
    let replay = world.hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(replay.durable);
    assert!(!replay.handed_off);
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert!(journal.projection.is_settled(&id));
    assert_dismissed_monitor_survives_store_reopen(world, report).await;
}

#[tokio::test]
async fn fenced_monitor_wake_without_obligation_is_retained_without_admission() {
    let world = MonitorWorld::new("fenced-fallback").await;
    let mut report = test_report(
        "fenced-fallback-report",
        "direct wake",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    let fence = world.hub.inner_monitor().stop_fences().fence(
        &world.session,
        &[],
        std::slice::from_ref(&report.monitor_id),
    );
    let receipt = world.hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(receipt.durable);
    assert!(!receipt.handed_off);
    assert_eq!(receipt.disposition, "follow_up_pending");
    assert!(
        crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap()
            .projection
            .pending
            .is_empty()
    );
    drop(fence);
    world.hub.inner_monitor().completion_cache().await.clear();
    let replay = world.hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(replay.durable);
    assert!(!replay.handed_off);
    assert_eq!(replay.disposition, "follow_up_pending");
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert!(
        journal
            .projection
            .is_settled("monitor:fenced-fallback-report")
    );
    let events = world.store.read(&world.session, 0, 256).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.payload.get("type").and_then(|v| v.as_str())
                == Some("monitor_report_pending"))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.payload.get("type").and_then(|v| v.as_str())
                == Some("monitor_report_delivered"))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event.payload.decode_event().is_ok_and(|payload| matches!(payload,
        EventPayload::UserMessage { text, mode, .. } if mode == DeliveryMode::Subturn && text.contains("monitor_event")
    ))), "dismissed direct report must never admit a wake");
    assert_dismissed_monitor_survives_store_reopen(world, report).await;
}

async fn assert_dismissed_monitor_survives_store_reopen(
    world: MonitorWorld,
    report: MonitorReport,
) {
    let MonitorWorld {
        hub,
        store,
        lease,
        _root: root,
        ..
    } = world;
    hub.inner_monitor().shutdown().await.unwrap();
    drop(lease);
    drop(hub);
    store.close().await.unwrap();
    let store = SqliteStoreHandle::open(root.path()).await.unwrap();
    let hub = SessionHub::new(
        store.clone(),
        crate::session_hub::SessionHubConfig::default(),
    )
    .unwrap();
    assert!(
        !hub.inner_monitor()
            .stop_fences()
            .session_fenced(&report.session_id)
    );
    let id = format!("monitor:{}", report.report_id);
    let receipt = hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(receipt.durable);
    assert!(!receipt.handed_off);
    assert_eq!(receipt.disposition, "follow_up_pending");
    let journal = crate::completion::load(&hub, &report.session_id)
        .await
        .unwrap();
    assert!(journal.projection.is_settled(&id));
    assert!(!journal.events.iter().any(|event| matches!(event,
        haider_protocol::completion::CompletionEvent::CompletionAdmitted { obligation_id, .. } if obligation_id == &id
    )));
    hub.inner_monitor().shutdown().await.unwrap();
    drop(hub);
    store.close().await.unwrap();
}

#[tokio::test]
async fn interrupted_handoff_replays_the_same_durable_attempt_and_admission() {
    use haider_protocol::completion::CompletionEvent;
    let world = MonitorWorld::new("completion-handoff").await;
    let mut report = test_report(
        "handoff-report",
        "one observation",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    report.occurrence = MonitorOccurrence::Once;
    observe_monitor(&world, report.clone()).await;
    for _ in 0..2 {
        world
            .hub
            .wake_monitor_report(report.clone())
            .await
            .expect_err("no worker manager: durable admission, interrupted handoff");
        world.hub.inner_monitor().completion_cache().await.clear();
    }
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    let pending = &journal.projection.pending["monitor:handoff-report"];
    assert_eq!(pending.attempt, 1);
    assert_eq!(pending.status, CompletionStatus::Admitting);
    assert!(pending.run_id.is_some());
    assert_eq!(
        journal
            .events
            .iter()
            .filter(|e| matches!(e, CompletionEvent::CompletionAttempt { .. }))
            .count(),
        1
    );
    assert_eq!(
        journal
            .events
            .iter()
            .filter(|e| matches!(e, CompletionEvent::CompletionAdmitted { .. }))
            .count(),
        1
    );
    assert!(
        !journal
            .events
            .iter()
            .any(|e| matches!(e, CompletionEvent::CompletionDelivered { .. }))
    );
}

#[tokio::test]
async fn first_delivery_keeps_source_outbox_order_and_two_busy_wakes_keep_their_ids() {
    let world = MonitorWorld::new("completion-outbox-order").await;
    world
        .lease
        .append(&mut [monitor_envelope(
            &world.session,
            Some(&world.run),
            None,
            None,
            "source-streaming",
            world.hub.device_id(),
            world.hub.worker_generation(),
            serde_json::to_value(EventPayload::RunState(RunState::Streaming)).unwrap(),
        )])
        .await
        .unwrap();
    let mut reports = Vec::new();
    for id in ["first", "coalesced-follow-up"] {
        let mut report = test_report(id, "one observation", MonitorReportStatus::Matched);
        report.session_id = world.session.clone();
        observe_monitor(&world, report.clone()).await;
        reports.push(report);
    }
    crate::completion::reconcile(&world.hub, &world.session)
        .await
        .unwrap();
    let queued = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(queued.projection.pending.len(), 2);
    assert!(
        queued
            .projection
            .pending
            .values()
            .all(|o| o.attempt == 0 && o.run_id.is_none()),
        "first delivery must remain owned by the ordered source outbox"
    );
    for report in reports {
        world
            .hub
            .wake_monitor_report(report)
            .await
            .expect_err("fixture has no worker manager");
    }
    let busy = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(busy.projection.pending.len(), 2);
    assert!(
        busy.projection
            .pending
            .values()
            .all(|o| o.attempt == 1 && o.run_id.as_ref() == Some(&world.run)),
        "both stable report IDs bind to the actual existing busy run: {:?}",
        busy.projection.pending
    );
}

#[tokio::test]
async fn cold_adoption_of_a_terminal_report_does_not_restart_its_source() {
    let world = MonitorWorld::new("completion-terminal-source").await;
    let mut registered = registration(None);
    registered.owner_session_id = world.session.clone();
    registered.source = MonitorSource::Timer {
        interval_ms: 86_400_000,
    };
    registered.occurrence = MonitorOccurrence::Once;
    world
        .hub
        .append(&mut [monitor_envelope(
            &world.session,
            None,
            None,
            None,
            "terminal-source-registered",
            world.hub.device_id(),
            world.hub.worker_generation(),
            serde_json::to_value(MonitorJournalEvent::MonitorRegistered {
                registration: registered,
            })
            .unwrap(),
        )])
        .await
        .unwrap();
    let mut report = test_report(
        "terminal-source-report",
        "already observed",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    report.occurrence = MonitorOccurrence::Once;
    report.source = MonitorSourceKind::Timer;
    observe_monitor(&world, report).await;
    let MonitorWorld {
        store,
        hub,
        session,
        lease,
        _root: root,
        ..
    } = world;
    hub.inner_monitor().shutdown().await.unwrap();
    drop(lease);
    drop(hub);
    store.close().await.unwrap();

    let reopened_store = SqliteStoreHandle::open(root.path()).await.unwrap();
    let reopened_hub = SessionHub::new(
        reopened_store.clone(),
        crate::session_hub::SessionHubConfig::default(),
    )
    .unwrap();
    let service = reopened_hub.inner_monitor();
    service
        .adopt_session(&reopened_hub, &session)
        .await
        .unwrap();
    assert!(
        service
            .inner
            .registry
            .has_terminal_pending(&session, "monitor-test")
    );
    assert!(
        service.inner.runner_tasks.lock().unwrap().is_empty(),
        "replaying the terminal report must not restart the source before its delivery receipt"
    );
    assert_eq!(
        service
            .inner
            .registry
            .pending_for_monitor(&session, "monitor-test")[0]
            .report
            .report_id,
        "terminal-source-report"
    );
    service.shutdown().await.unwrap();
    drop(reopened_hub);
    reopened_store.close().await.unwrap();
}

#[tokio::test]
async fn verifier_deleted_session_retires_cached_incomplete_admission() {
    use haider_protocol::completion::CompletionEvent;
    let world = MonitorWorld::new("verifier-delete-admitting").await;
    let mut report = test_report(
        "verifier-delete-admitting-report",
        "pending admission",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    observe_monitor(&world, report).await;
    world
        .lease
        .append(&mut [monitor_envelope(
            &world.session,
            Some(&world.run),
            None,
            None,
            "verifier-source-terminal",
            world.hub.device_id(),
            world.hub.worker_generation(),
            serde_json::to_value(EventPayload::RunState(RunState::Done)).unwrap(),
        )])
        .await
        .unwrap();
    let service = world.hub.inner_monitor().clone();
    let guard = service.completion_lock().await;
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    let obligation = journal.projection.pending.values().next().unwrap();
    crate::completion::append(
        &world.hub,
        obligation,
        CompletionEvent::CompletionAttempt {
            obligation_id: obligation.obligation_id.clone(),
            attempt: 1,
            worker_generation: world.hub.worker_generation(),
        },
    )
    .await
    .unwrap();
    // The durable prefix exists after an attempt/admission crash. A cold
    // reconciler reads it before dispatch; deletion can win before admission.
    service.completion_cache().await.clear();
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(
        journal.projection.pending.values().next().unwrap().status,
        CompletionStatus::Admitting
    );
    let MonitorWorld {
        hub,
        store,
        session,
        lease,
        _root: root,
        ..
    } = world;
    drop(lease);
    hub.delete_session(session.clone()).await.unwrap();
    drop(guard);
    let first = crate::completion::reconcile(&hub, &session).await;
    let second = crate::completion::reconcile(&hub, &session).await;
    assert!(!hub.session_ids().await.unwrap().contains(&session));
    assert!(
        first.is_ok() && second.is_ok(),
        "deleted sessions must leave the retry scheduler, first={first:?}, second={second:?}"
    );
    hub.inner_monitor().shutdown().await.unwrap();
    drop(hub);
    store.close().await.unwrap();
    drop(root);
}

#[test]
fn verifier_delete_keeps_monitor_fence_until_cache_eviction() {
    // A single blocking thread makes the durable-absence read a FIFO barrier:
    // the prior SQLite delete task has completed and published its JoinHandle
    // result before the absence query can execute. No timing sleep is needed.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build().unwrap().block_on(async {
    use haider_protocol::completion::CompletionEvent;
    let world = MonitorWorld::new("verifier-delete-admitting").await;
    let mut report = test_report(
        "verifier-delete-admitting-report",
        "pending admission",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    observe_monitor(&world, report).await;
    world
        .lease
        .append(&mut [monitor_envelope(
            &world.session,
            Some(&world.run),
            None,
            None,
            "verifier-source-terminal",
            world.hub.device_id(),
            world.hub.worker_generation(),
            serde_json::to_value(EventPayload::RunState(RunState::Done)).unwrap(),
        )])
        .await
        .unwrap();
    let service = world.hub.inner_monitor().clone();
    let guard = service.completion_lock().await;
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    let obligation = journal.projection.pending.values().next().unwrap();
    crate::completion::append(
        &world.hub,
        obligation,
        CompletionEvent::CompletionAttempt {
            obligation_id: obligation.obligation_id.clone(),
            attempt: 1,
            worker_generation: world.hub.worker_generation(),
        },
    )
    .await
    .unwrap();
    // The durable prefix exists after an attempt/admission crash. A cold
    // reconciler reads it before dispatch; deletion can win before admission.
    service.completion_cache().await.clear();
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert_eq!(
        journal.projection.pending.values().next().unwrap().status,
        CompletionStatus::Admitting
    );
    let MonitorWorld {
        hub,
        store,
        session,
        lease,
        _root: root,
        ..
    } = world;
    drop(lease);
    // Force durable deletion to finish while eviction is blocked on its real
    // mutex. A released tombstone here exposes the stale admitting cache.
    let fenced = {
        let cache_guard = hub.inner_monitor().completion_cache().await;
        let deleting = hub.delete_session(session.clone());
        tokio::pin!(deleting);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut deleting => panic!("delete completed while cache mutex was held: {result:?}"),
            () = async {
                loop {
                    if !hub.session_ids().await.unwrap().contains(&session) { break; }
                    tokio::task::yield_now().await;
                }
            } => (),
        }
    }).await.unwrap();
        // SQLite has completed deletion. Poll that same future once explicitly so
        // it consumes the durable result and reaches its blocked cache eviction.
        std::future::poll_fn(|cx| {
            use std::future::Future;
            assert!(deleting.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let fenced = hub.inner_monitor().completion_retired(&session);
        assert!(cache_guard.contains_key(&session));
        drop(cache_guard);
        deleting.await.unwrap();
        fenced
    };
    assert!(
        fenced,
        "monitor tombstone released while stale completion cache still exists after durable deletion"
    );
    drop(guard);
    let first = crate::completion::reconcile(&hub, &session).await;
    let second = crate::completion::reconcile(&hub, &session).await;
    assert!(!hub.session_ids().await.unwrap().contains(&session));
    assert!(
        first.is_ok() && second.is_ok(),
        "deleted sessions must leave the retry scheduler, first={first:?}, second={second:?}"
    );
    hub.inner_monitor().shutdown().await.unwrap();
    drop(hub);
    store.close().await.unwrap();
    drop(root);
    });
}

#[cfg(unix)]
#[tokio::test]
async fn direct_report_stop_epoch_dismissal_survives_fence_release_and_replay() {
    let world = MonitorWorld::new("completion-direct-epoch").await;
    let mut report = test_report(
        "direct-epoch-report",
        "old direct observation",
        MonitorReportStatus::Matched,
    );
    report.session_id = world.session.clone();
    let (arrived, resume) = crate::completion::astra22_boundary_hook::install(&world.session);
    let hub = world.hub.clone();
    let pending_report = report.clone();
    let wake = tokio::spawn(async move { hub.wake_monitor_report(pending_report).await });
    tokio::time::timeout(std::time::Duration::from_secs(20), arrived)
        .await
        .unwrap()
        .unwrap();
    {
        let monitor = world.hub.inner_monitor();
        let _admission = monitor.admission_lock().await;
        let fence = monitor.stop_fences().fence(
            &world.session,
            &[],
            std::slice::from_ref(&report.monitor_id),
        );
        drop(fence);
    }
    resume.send(()).unwrap();
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(20), wake)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(receipt.durable);
    assert!(!receipt.handed_off);
    assert_eq!(receipt.disposition, "follow_up_pending");
    world.hub.inner_monitor().completion_cache().await.clear();
    let replay = world.hub.wake_monitor_report(report.clone()).await.unwrap();
    assert!(!replay.handed_off);
    let journal = crate::completion::load(&world.hub, &world.session)
        .await
        .unwrap();
    assert!(journal.projection.is_settled("monitor:direct-epoch-report"));
    assert!(!journal.events.iter().any(|event| matches!(event,
        haider_protocol::completion::CompletionEvent::CompletionAdmitted { obligation_id, .. }
            if obligation_id == "monitor:direct-epoch-report"
    )));
    assert!(
        world
            .store
            .read(&world.session, 0, 256)
            .await
            .unwrap()
            .iter()
            .all(|event| !event.event_id.as_str().starts_with("monitor-user-"))
    );
    world.hub.inner_monitor().shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn follow_up_waiting_on_completion_does_not_block_registry_operations() {
    let world = MonitorWorld::new("completion-lock-order").await;
    let id = pending_task(&world).await;
    let monitor = world.hub.inner_monitor();
    let completion = monitor.completion_lock().await;
    let (arrived, resume) = crate::completion::astra22_boundary_hook::install(&world.session);
    let hub = world.hub.clone();
    let lease = world.lease.clone();
    let coordinates = world.coordinates("dismiss-during-wake");
    let control = tokio::spawn(async move {
        hub.execute_monitor_tool(
            &lease,
            coordinates,
            MonitorRequest::FollowUp {
                obligation_id: id,
                action: CompletionControl::Dismiss,
                attempt: 0,
                evidence_event_ids: Vec::new(),
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), arrived)
        .await
        .unwrap()
        .unwrap();
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        monitor.client_list(&world.hub, world.session.clone()),
    )
    .await
    .expect("registry listing must finish while completion is held");
    assert!(matches!(
        listed.outcome,
        haider_rpc::MonitorListOutcomeWire::Listed { .. }
    ));
    resume.send(()).unwrap();
    drop(completion);
    tokio::time::timeout(std::time::Duration::from_secs(20), control)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        crate::completion::load(&world.hub, &world.session)
            .await
            .unwrap()
            .projection
            .is_settled("task:completion-task")
    );
    monitor.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_dismisses_expired_monitor_follow_up_before_cold_projection_resume() {
    for run_bound in [true, false] {
        let label = format!("completion-expired-stop-run-bound-{run_bound}");
        let world = MonitorWorld::new(&label).await;
        let monitor = world.hub.inner_monitor();
        let completion = monitor.completion_lock().await;
        let mut report = test_report(
            "expired-stop-report",
            "old one-shot report",
            MonitorReportStatus::Matched,
        );
        report.session_id = world.session.clone();
        report.occurrence = MonitorOccurrence::Once;
        let mut registered = registration(None);
        registered.owner_session_id = world.session.clone();
        registered.monitor_id = report.monitor_id.clone();
        world
            .hub
            .append(&mut [monitor_envelope(
                &world.session,
                if run_bound { Some(&world.run) } else { None },
                None,
                None,
                "expired-monitor-registration",
                world.hub.device_id(),
                world.hub.worker_generation(),
                MonitorJournalEvent::MonitorRegistered {
                    registration: registered,
                }
                .to_value()
                .unwrap(),
            )])
            .await
            .unwrap();
        observe_monitor(&world, report.clone()).await;
        world
            .hub
            .append(&mut [monitor_envelope(
                &world.session,
                if run_bound { Some(&world.run) } else { None },
                None,
                None,
                "expired-monitor-auto-removal",
                world.hub.device_id(),
                world.hub.worker_generation(),
                MonitorJournalEvent::MonitorRemoved {
                    monitor_id: report.monitor_id.clone(),
                    reason: MonitorRemovalReason::OneShotComplete,
                    removed_at_ms: now_ms(),
                }
                .to_value()
                .unwrap(),
            )])
            .await
            .unwrap();
        let listed = monitor.client_list(&world.hub, world.session.clone()).await;
        assert!(
            matches!(listed.outcome, haider_rpc::MonitorListOutcomeWire::Listed { ref monitors } if monitors.is_empty())
        );
        assert!(
            !crate::completion::load(&world.hub, &world.session)
                .await
                .unwrap()
                .projection
                .is_settled("monitor:expired-stop-report")
        );
        let admission = monitor.admission_lock().await;
        let captured_monitors = monitor
            .stop_monitor_ids(&world.hub, &world.session, &admission)
            .await
            .unwrap();
        if !run_bound {
            assert!(captured_monitors.contains(&report.monitor_id));
        }
        drop(admission);
        let incarnation = crate::delegation::StopIncarnation {
            session_id: world.session.clone(),
            runs: vec![world.run.clone()],
            monitors: if run_bound {
                Vec::new()
            } else {
                captured_monitors
            },
        };
        assert_eq!(
            monitor
                .remove_for_stop_incarnation(
                    &world.hub,
                    &world.session,
                    "expired-stop",
                    &incarnation
                )
                .await
                .unwrap(),
            0
        );
        drop(completion);
        monitor.completion_cache().await.clear();
        let receipt = world.hub.wake_monitor_report(report).await.unwrap();
        assert!(receipt.durable && !receipt.handed_off);
        assert!(
            crate::completion::load(&world.hub, &world.session)
                .await
                .unwrap()
                .projection
                .is_settled("monitor:expired-stop-report")
        );
        assert!(
            world
                .store
                .read(&world.session, 0, 256)
                .await
                .unwrap()
                .iter()
                .all(|event| !event.event_id.as_str().starts_with("monitor-user-"))
        );
        monitor.shutdown().await.unwrap();
    }
}
