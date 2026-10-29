#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::monitor_tests::MonitorWorld;
use super::*;
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};

fn target(world: &MonitorWorld) -> crate::delegation::StopIncarnation {
    crate::delegation::StopIncarnation {
        session_id: world.session.clone(),
        runs: vec![world.run.clone()],
        monitors: Vec::new(),
    }
}

async fn seed_legacy_watch(world: &MonitorWorld, call: &str) -> MonitorRegistration {
    let id = stable_digest(&[
        world.session.as_str(),
        world.run.as_str(),
        call,
        "monitor-tool",
    ]);
    let registration = MonitorRegistration {
        monitor_id: format!("monitor-{}", &id[..20]),
        owner_session_id: world.session.clone(),
        source: MonitorSource::Timer {
            interval_ms: 3_600_000,
        },
        filter: None,
        action: MonitorAction {
            report: true,
            follow_up: Some("original watch follow-up".into()),
        },
        occurrence: MonitorOccurrence::Every,
        created_at_ms: 1,
        activated_at_ms: 1,
        start_sequence: 0,
        expires_at_ms: None,
        branch_id: None,
        agent_id: None,
        paused: false,
        exited: false,
        workspace_root: None,
        approved_command: None,
        approved_file_path: None,
    };
    // The normal dispatcher journals the call before execution. This is the
    // old registration shape, including the absent envelope run ID.
    let started = haider_protocol::EventPayload::Item(ItemEvent::Started {
        item_id: haider_protocol::ids::ItemId::new(format!("{call}-item")),
        item: TurnItem::ToolCall {
            call_id: call.into(),
            name: "monitor".into(),
            args: json!({}),
            status: ToolStatus::InProgress,
            activity: None,
        },
    });
    let mut envelopes = vec![monitor_envelope(
        &world.session,
        Some(&world.run),
        None,
        None,
        &format!("{call}-started"),
        world.hub.device_id(),
        world.hub.worker_generation(),
        serde_json::to_value(started).expect("item"),
    )];
    // Cross a journal page before reaching the original registration.
    for index in 0..260 {
        envelopes.push(monitor_envelope(
            &world.session,
            None,
            None,
            None,
            &format!("{call}-padding-{index}"),
            world.hub.device_id(),
            world.hub.worker_generation(),
            json!({"type":"padding"}),
        ));
    }
    envelopes.push(monitor_envelope(
        &world.session,
        None,
        None,
        None,
        &format!("{call}-registered"),
        world.hub.device_id(),
        world.hub.worker_generation(),
        MonitorJournalEvent::MonitorRegistered {
            registration: registration.clone(),
        }
        .to_value()
        .expect("registration"),
    ));
    world
        .hub
        .append(&mut envelopes)
        .await
        .expect("legacy journal");
    registration
}

#[tokio::test]
async fn r8_tool_watch_records_original_task() {
    let world = MonitorWorld::new("original-task").await;
    let mut expected = HashSet::new();
    let mut revised = None;
    for (call, occurrence, lifetime) in [
        (
            "session-watch",
            MonitorOccurrence::Every,
            MonitorLifetime::Session,
        ),
        (
            "timeout-watch",
            MonitorOccurrence::Every,
            MonitorLifetime::Timeout {
                timeout_ms: 3_600_000,
            },
        ),
        (
            "once-watch",
            MonitorOccurrence::Once,
            MonitorLifetime::Session,
        ),
    ] {
        let result = world
            .execute(
                call,
                MonitorRequest::Register {
                    source: MonitorSource::Timer {
                        interval_ms: 3_600_000,
                    },
                    filter: None,
                    action: MonitorAction {
                        report: true,
                        follow_up: None,
                    },
                    occurrence,
                    lifetime,
                },
            )
            .await;
        assert_eq!(result.status, ToolResultStatus::Completed);
        let id = serde_json::from_str::<serde_json::Value>(&result.preview)
            .expect("registration preview")["monitor_id"]
            .as_str()
            .expect("monitor ID")
            .to_owned();
        if call == "once-watch" {
            revised = Some(id.clone());
        }
        expected.insert(id);
    }
    let monitor_id = revised.expect("watch to revise");
    for (call, request) in [
        (
            "pause-once",
            MonitorRequest::Pause {
                monitor_id: monitor_id.clone(),
            },
        ),
        ("resume-once", MonitorRequest::Resume { monitor_id }),
    ] {
        assert_eq!(
            world.execute(call, request).await.status,
            ToolResultStatus::Completed
        );
    }
    let events = world
        .store
        .read(&world.session, 0, 4096)
        .await
        .expect("journal");
    let originals = events
        .iter()
        .filter(|event| event.payload.get("monitor_owner_run_id").is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        originals.len(),
        3,
        "each original registration needs its task coordinate"
    );
    for original in originals {
        assert!(original.run_id.is_none());
        assert_eq!(
            original
                .payload
                .get("monitor_owner_run_id")
                .and_then(serde_json::Value::as_str),
            Some(world.run.as_str())
        );
    }
    let owned = world
        .hub
        .inner_monitor()
        .stop_owned_monitor_ids(&world.hub, &target(&world))
        .await
        .expect("owned watches");
    assert_eq!(
        owned, expected,
        "all original watches retain ownership across lifetimes and revisions"
    );
    world.hub.shutdown().await.expect("shutdown");
    world.store.close().await.expect("close");
}

#[tokio::test]
async fn r8_legacy_original_watch_survives_update_as_owned() {
    let world = MonitorWorld::new("legacy-update").await;
    let mut registration = seed_legacy_watch(&world, "script-child-watch").await;
    registration.activated_at_ms = 2;
    world
        .hub
        .append(&mut [monitor_envelope(
            &world.session,
            Some(&RunId::new("later-updater")),
            None,
            None,
            "updated",
            world.hub.device_id(),
            world.hub.worker_generation(),
            MonitorJournalEvent::MonitorRegistered {
                registration: registration.clone(),
            }
            .to_value()
            .expect("update"),
        )])
        .await
        .expect("updated registration");
    let service = world.hub.inner_monitor();
    let owned = service
        .stop_owned_monitor_ids(&world.hub, &target(&world))
        .await
        .expect("owned watches");
    assert!(owned.contains(&registration.monitor_id));
    let admission = service.admission_lock().await;
    let fence = service
        .fence_stop_incarnation(&world.hub, &target(&world), &admission)
        .await
        .expect("fence");
    assert!(
        service
            .stop_fences()
            .report_fenced(&world.session, &registration.monitor_id)
    );
    drop(admission);
    drop(fence);
    world.hub.shutdown().await.expect("shutdown");
    world.store.close().await.expect("close");
}

#[tokio::test]
async fn r8_independent_watch_update_does_not_transfer_ownership() {
    let world = MonitorWorld::new("independent-update").await;
    let mut registration = seed_legacy_watch(&world, "owned-call").await;
    registration.monitor_id = "independent-client-watch".into();
    for (suffix, run) in [("original", None), ("update", Some(&world.run))] {
        world
            .hub
            .append(&mut [monitor_envelope(
                &world.session,
                run,
                None,
                None,
                &format!("independent-{suffix}"),
                world.hub.device_id(),
                world.hub.worker_generation(),
                MonitorJournalEvent::MonitorRegistered {
                    registration: registration.clone(),
                }
                .to_value()
                .expect("registration"),
            )])
            .await
            .expect("independent journal");
    }
    let service = world.hub.inner_monitor();
    assert!(
        !service
            .stop_owned_monitor_ids(&world.hub, &target(&world))
            .await
            .expect("owned watches")
            .contains(&registration.monitor_id)
    );
    let mut captured = target(&world);
    captured.monitors.push(registration.monitor_id.clone());
    assert!(
        service
            .stop_owned_monitor_ids(&world.hub, &captured)
            .await
            .expect("captured watches")
            .contains(&registration.monitor_id)
    );
    world.hub.shutdown().await.expect("shutdown");
    world.store.close().await.expect("close");
}

async fn terminal_pending(reason: MonitorRemovalReason) {
    let world = MonitorWorld::new(&format!("terminal-{reason:?}")).await;
    let registration = seed_legacy_watch(&world, "terminal-watch").await;
    let report = MonitorReport {
        report_id: "terminal-report".into(),
        monitor_id: registration.monitor_id.clone(),
        session_id: world.session.clone(),
        branch_id: None,
        agent_id: None,
        source: MonitorSourceKind::Timer,
        occurrence: MonitorOccurrence::Once,
        status: MonitorReportStatus::Matched,
        events: vec![],
        coalesced_count: 1,
        omitted_count: 0,
        action: registration.action.clone(),
    };
    let mut envelopes = [
        monitor_envelope(
            &world.session,
            None,
            None,
            None,
            "terminal-pending",
            world.hub.device_id(),
            world.hub.worker_generation(),
            MonitorJournalEvent::MonitorReportPending {
                pending: PendingMonitorReport {
                    report: report.clone(),
                    terminal_reason: Some(reason),
                    queue_order: 1,
                    queued_at_ms: 1,
                },
            }
            .to_value()
            .expect("pending"),
        ),
        monitor_envelope(
            &world.session,
            None,
            None,
            None,
            "terminal-removed",
            world.hub.device_id(),
            world.hub.worker_generation(),
            MonitorJournalEvent::MonitorRemoved {
                monitor_id: registration.monitor_id.clone(),
                reason,
                removed_at_ms: 2,
            }
            .to_value()
            .expect("removal"),
        ),
    ];
    world
        .hub
        .append(&mut envelopes)
        .await
        .expect("terminal journal");
    let service = world.hub.inner_monitor();
    // The source has already left the live registry. The owed report remains.
    assert!(service.inner.registry.snapshot(&world.session).is_empty());
    let admission = service.admission_lock().await;
    let fence = service
        .fence_stop_incarnation(&world.hub, &target(&world), &admission)
        .await
        .expect("terminal fence");
    drop(admission);
    service
        .remove_for_stop_incarnation(&world.hub, &world.session, "terminal-stop", &target(&world))
        .await
        .expect("settle report");
    assert!(
        crate::completion::load(&world.hub, &world.session)
            .await
            .expect("completion")
            .projection
            .is_settled("monitor:terminal-report")
    );
    crate::completion::wake_monitor(&world.hub, report)
        .await
        .expect("settled delivery");
    assert!(
        !world
            .store
            .read(&world.session, 0, 4096)
            .await
            .expect("journal")
            .iter()
            .any(|e| e.event_id.as_str().starts_with("monitor-user-"))
    );
    drop(fence);
    world.hub.shutdown().await.expect("shutdown");
    world.store.close().await.expect("close");
}

#[tokio::test]
async fn r8_expired_watch_pending_report_is_settled() {
    terminal_pending(MonitorRemovalReason::TimedOut).await;
}

#[tokio::test]
async fn r8_one_shot_watch_pending_report_is_settled() {
    terminal_pending(MonitorRemovalReason::OneShotComplete).await;
}
