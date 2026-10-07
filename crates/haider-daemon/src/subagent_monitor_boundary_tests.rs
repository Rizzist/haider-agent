use super::*;

// Astra's ordering regressions and controls, retained as library tests.
#[cfg(unix)]
async fn astra22_queued_monitor_boundary(scope: &str, once: bool, stale_snapshot: bool) {
    let label = format!(
        "a22-queue-{scope}-{}-{}",
        if once { "once" } else { "every" },
        if stale_snapshot { "snapshot" } else { "queued" }
    );
    let harness = start_parked_child_with_wait_budgets(
        &label,
        ParkedChildMode::ParentWaitTimeoutHang,
        Duration::from_secs(1),
        Duration::from_secs(20),
        Duration::from_millis(250),
        Some(2_250),
    )
    .await;
    let child = harness.child.clone();
    let monitor = harness.hub.inner_monitor();
    let registered = monitor
        .client_register(
            &harness.hub,
            crate::monitor::MonitorClientRegistrationRequest {
                command_id: haider_rpc::CommandId::new(format!("{label}-register")),
                session_id: child.child_session_id.clone(),
                worker_generation: harness.hub.worker_generation(),
                source: haider_rpc::MonitorSourceWire::Sms,
                filter: None,
                action: haider_rpc::MonitorActionWire {
                    report: true,
                    follow_up: None,
                },
                occurrence: if once {
                    haider_rpc::MonitorOccurrenceWire::Once
                } else {
                    haider_rpc::MonitorOccurrenceWire::Every
                },
                lifetime: haider_rpc::MonitorLifetimeWire::Session,
            },
        )
        .await;
    assert!(
        matches!(
            registered.outcome,
            haider_rpc::MonitorRegisterOutcomeWire::Registered { .. }
        ),
        "{registered:?}"
    );
    // A different completion operation can own this existing production lock.
    // Hold it before the report arrives, then stop after durable outbox enqueue.
    let mut completion_guard = Some(monitor.completion_lock().await);
    let barrier = stale_snapshot
        .then(|| crate::completion::astra22_boundary_hook::install(&child.child_session_id));
    crate::monitor::publish_sms_incoming(
        &harness.hub.monitor_source_hub(),
        "+15550000000",
        "synthetic boundary probe",
        1,
    )
    .expect("synthetic event");
    timeout(Duration::from_secs(20), async {
        loop {
            let events = harness
                .store
                .read(&child.child_session_id, 0, 2048)
                .await
                .expect("child events");
            if events.iter().any(|e| {
                e.payload.get("type").and_then(|v| v.as_str()) == Some("monitor_report_pending")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("durable pending report before stop");
    let resume = if let Some((arrived, resume)) = barrier {
        drop(completion_guard.take());
        timeout(Duration::from_secs(20), arrived)
            .await
            .expect("wake reached post-attempt barrier")
            .expect("barrier sender");
        Some(resume)
    } else {
        None
    };
    let coordinator = crate::delegation::DelegationHandle::new(harness.hub.clone());
    if scope == "main" {
        let request_json = serde_json::json!({"session_id": harness.parent_session, "run_id": child.parent_run_id, "reason": "manual"}).to_string();
        harness
            .hub
            .cancel_internal_turn(TurnCancelCommand {
                command_id: format!("{label}-main-cancel"),
                request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json,
                session_id: harness.parent_session.clone(),
                worker_generation: harness.hub.worker_generation(),
                run_id: child.parent_run_id.clone(),
                cancelling_event_id: EventId::new(format!("{label}-main-cancelling")),
                device_id: harness.hub.device_id(),
            })
            .await
            .expect("main-only stop must preserve child monitor wake");
    } else if scope == "tree" {
        timeout(
            Duration::from_secs(30),
            coordinator.stop_subagent_tree(
                crate::delegation::SubagentTreeRoot::Session(harness.parent_session.clone()),
                &label,
            ),
        )
        .await
        .expect("tree stop while delivery queued")
        .expect("tree stop");
    } else if scope == "named" {
        let request_json = serde_json::json!({"session_id": child.child_session_id, "run_id": child.child_run_id, "reason": "manual"}).to_string();
        timeout(
            Duration::from_secs(30),
            coordinator.cancel_single_subagent(
                &child,
                TurnCancelCommand {
                    command_id: format!("{label}-cancel"),
                    request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                    request_json,
                    session_id: child.child_session_id.clone(),
                    worker_generation: harness.hub.worker_generation(),
                    run_id: child.child_run_id.clone(),
                    cancelling_event_id: EventId::new(format!("{label}-cancelling")),
                    device_id: harness.hub.device_id(),
                },
                &label,
            ),
        )
        .await
        .expect("named stop while delivery queued")
        .expect("named stop");
    }
    timeout(Duration::from_secs(30), async {
        while monitor
            .stop_fences()
            .session_fenced(&child.child_session_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stop fence released");
    let before = harness
        .store
        .read(&child.child_session_id, 0, 2048)
        .await
        .expect("before release");
    let monitor_users = |events: &[RawEnvelope]| {
        events
            .iter()
            .filter_map(|e| {
                if e.event_id.as_str().starts_with("monitor-user-") {
                    e.run_id.as_ref().map(ToString::to_string)
                } else if e.payload.get("type").and_then(|v| v.as_str())
                    == Some("completion_admitted")
                    && e.payload
                        .get("obligation_id")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id.starts_with("monitor:"))
                {
                    e.payload
                        .get("run_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                } else {
                    None
                }
            })
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    };
    assert_eq!(
        monitor_users(&before),
        0,
        "lock must block monitor admission"
    );
    drop(completion_guard.take());
    if let Some(resume) = resume {
        resume.send(()).expect("resume wake after completed stop");
    }
    let mut after = before.clone();
    for _ in 0..500 {
        after = harness
            .store
            .read(&child.child_session_id, 0, 2048)
            .await
            .expect("after release");
        if monitor_users(&after) > 0
            || after.iter().any(|e| {
                e.payload.get("type").and_then(|v| v.as_str()) == Some("monitor_report_delivered")
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let admitted = monitor_users(&after);
    let delivery_observed = admitted > 0
        || after.iter().any(|e| {
            e.payload.get("type").and_then(|v| v.as_str()) == Some("monitor_report_delivered")
        });
    let timeline = after
        .iter()
        .filter(|e| {
            e.payload
                .get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| {
                    t.starts_with("monitor_")
                        || t.starts_with("completion_")
                        || t == "run_state"
                        || t == "user_message"
                })
        })
        .map(|e| serde_json::json!({"seq":e.seq,"run":e.run_id,"payload":e.payload}))
        .collect::<Vec<_>>();
    println!(
        "ASTRA22_BOUNDARY {}",
        serde_json::json!({"scope":scope,"once":once,"stale_snapshot":stale_snapshot,"monitor_runs_admitted_after_release":admitted,"delivery_observed":delivery_observed,"timeline":timeline})
    );
    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
    assert!(
        delivery_observed,
        "no completed report delivery observed; do not count a vacuous pass"
    );
    if scope == "control" || scope == "main" {
        assert!(
            admitted > 0,
            "negative control must admit the real queued report"
        );
    } else {
        assert_eq!(
            admitted, 0,
            "a pre-stop durable monitor report must not create work after the stop settles"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_control_every() {
    astra22_queued_monitor_boundary("control", false, false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_after_tree_stop_every() {
    astra22_queued_monitor_boundary("tree", false, false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_after_named_stop_every() {
    astra22_queued_monitor_boundary("named", false, false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_after_tree_stop_once() {
    astra22_queued_monitor_boundary("tree", true, false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_after_named_stop_once() {
    astra22_queued_monitor_boundary("named", true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_stale_snapshot_control() {
    astra22_queued_monitor_boundary("control", false, true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_stale_snapshot_tree_every() {
    astra22_queued_monitor_boundary("tree", false, true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_stale_snapshot_named_every() {
    astra22_queued_monitor_boundary("named", false, true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_stale_snapshot_tree_once() {
    astra22_queued_monitor_boundary("tree", true, true).await;
}
#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_stale_snapshot_named_once() {
    astra22_queued_monitor_boundary("named", true, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_main_only_every_queued() {
    astra22_queued_monitor_boundary("main", false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_main_only_every_snapshot() {
    astra22_queued_monitor_boundary("main", false, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_main_only_once_queued() {
    astra22_queued_monitor_boundary("main", true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn astra22_queued_monitor_main_only_once_snapshot() {
    astra22_queued_monitor_boundary("main", true, true).await;
}
