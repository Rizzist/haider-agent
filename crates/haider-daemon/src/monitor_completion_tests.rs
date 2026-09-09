use super::*;
use haider_protocol::completion::{CompletionSource, CompletionStatus};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
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
