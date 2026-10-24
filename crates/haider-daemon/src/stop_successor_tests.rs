//! Ordered public-stop regressions: a durable claim never adopts successor work.
use super::*;
use haider_core::{DelegationRecord, DelegationState};
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::ids::LeaseId;
use haider_tools::{
    MonitorAction, MonitorLifetime, MonitorOccurrence, MonitorRequest, MonitorSource,
};
use std::time::Duration;

async fn successor_report(
    hub: &SessionHub,
    session: &SessionId,
    successor: &RunId,
    label: &str,
    direct: bool,
) -> crate::monitor::MonitorReport {
    // Client registration creates an independent watch even while old tasks settle.
    let registered = hub
        .inner_monitor()
        .client_register(
            hub,
            crate::monitor::MonitorClientRegistrationRequest {
                command_id: haider_rpc::CommandId::new(format!("{label}-watch")),
                session_id: session.clone(),
                worker_generation: hub.worker_generation(),
                source: haider_rpc::MonitorSourceWire::Timer {
                    interval_ms: 3_600_000,
                },
                filter: None,
                action: haider_rpc::MonitorActionWire {
                    report: true,
                    follow_up: Some("later work".into()),
                },
                occurrence: haider_rpc::MonitorOccurrenceWire::Every,
                lifetime: haider_rpc::MonitorLifetimeWire::Session,
            },
        )
        .await;
    let monitor_id = match registered.outcome {
        haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } => monitor.monitor_id,
        other => panic!("{other:?}"),
    };
    let report = crate::monitor::MonitorReport {
        report_id: format!("{label}-report"),
        monitor_id: monitor_id.clone(),
        session_id: session.clone(),
        branch_id: None,
        agent_id: None,
        source: haider_tools::MonitorSourceKind::Timer,
        occurrence: MonitorOccurrence::Every,
        status: crate::monitor::MonitorReportStatus::Matched,
        events: vec![],
        coalesced_count: 1,
        omitted_count: 0,
        action: MonitorAction {
            report: true,
            follow_up: Some("later work".into()),
        },
    };
    if !direct {
        let mut pending = run_state_envelope(
            session,
            successor,
            hub.worker_generation(),
            &format!("{label}-pending"),
            RunState::Queued,
        );
        *pending.payload = serde_json::json!({"type":"monitor_report_pending","pending":{"report":report,"queue_order":1,"queued_at_ms":1}});
        hub.append(&mut [pending]).await.expect("durable report");
    }
    report
}

async fn successor_during_stop(scope: &str, recovery: bool, direct: bool) {
    let label = format!("successor-{scope}-{recovery}-{direct}");
    let root = tempfile::tempdir().expect("profile");
    let mut store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let parent = SessionId::new(format!("{label}-parent"));
    let child = SessionId::new(format!("{label}-child"));
    let old = RunId::new(format!("{label}-main"));
    let child_run = RunId::new(format!("{label}-helper"));
    let agent = AgentId::new(format!("{label}-agent"));
    for (session, run, suffix) in [(&parent, &old, "parent"), (&child, &child_run, "child")] {
        hub.create_internal_session(create_command(session, &format!("{label}-{suffix}")))
            .await
            .expect("session");
        hub.accept_turn(accept_command(
            session,
            run,
            hub.worker_generation(),
            &format!("{label}-{suffix}"),
        ))
        .await
        .expect("turn");
    }
    let mut configured = run_state_envelope(
        &parent,
        &old,
        hub.worker_generation(),
        &format!("{label}-configured"),
        RunState::Queued,
    );
    *configured.payload = HeadlessRunEventPayload::HeadlessRunConfigured(HeadlessRunSpecV1 {
        agent_spawn: None,
        continuation_of: None,
        cwd: root.path().display().to_string(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_output_tokens: 64,
        effort: None,
        fast: false,
        seed: None,
        permission_overrides: Default::default(),
        trust_hooks: false,
        budget: RunBudgetV1::default(),
        request_deadline_unix_ms: None,
        replay_of: None,
    })
    .to_payload_value()
    .expect("config");
    hub.append(&mut [configured])
        .await
        .expect("headless config");
    let record = DelegationRecord {
        agent_id: agent.clone(),
        child_session_id: child.clone(),
        child_run_id: child_run.clone(),
        parent_session_id: parent.clone(),
        parent_run_id: old.clone(),
        parent_branch_id: None,
        call_id: format!("{label}-spawn"),
        tool_item_id: ItemId::new(format!("{label}-spawn-item")),
        parent_agent_id: None,
        root_session_id: parent.clone(),
        depth: 1,
        task: label.clone(),
        prompt: label.clone(),
        manifest: AgentManifest {
            agent: agent.clone(),
            role: AgentRole::Subagent,
            task: label.clone(),
            callsign: None,
            model_profile: "fake-v1".into(),
            grant: Grant {
                tools: vec![],
                effect_ceiling: vec![],
            },
            budget_tokens: None,
            placement: Placement::Local,
            lease: LeaseId::new(format!("{label}-lease")),
            fencing_epoch: hub.worker_generation(),
            attempt: 0,
            parent: None,
            coordinates: None,
            cli_scope: None,
        },
        state: DelegationState::Spawned,
        report: None,
    };
    hub.create_delegation(record.clone())
        .await
        .expect("delegation");
    let command_id = haider_rpc::CommandId::new(format!("{label}-stop"));
    let request_json = serde_json::to_string(&match scope {
        "named" => serde_json::json!({"operation":"agent.cancel", "session_id":parent, "agent":agent}),
        "tree" => serde_json::json!({"operation":"agent.cancel_all", "session_id":parent}),
        _ => serde_json::json!({"operation":"headless.run.stop", "session_id":parent, "run_id":old, "scope":haider_rpc::RunStopScopeWire::All}),
    }).expect("request json");
    if recovery {
        let delegation = crate::delegation::DelegationHandle::new(hub.clone());
        let monitor = hub.inner_monitor();
        let admission = monitor.admission_lock().await;
        let intent = if scope == "named" {
            serde_json::to_value(
                delegation
                    .delegation_stop_incarnation(&record, &admission)
                    .await
                    .expect("capture named"),
            )
        } else {
            let tree = if scope == "tree" {
                crate::delegation::SubagentTreeRoot::Session(parent.clone())
            } else {
                crate::delegation::SubagentTreeRoot::Run {
                    session_id: parent.clone(),
                    run_id: old.clone(),
                }
            };
            serde_json::to_value(
                delegation
                    .stop_incarnations(&tree, &admission)
                    .await
                    .expect("capture tree"),
            )
        }
        .expect("intent json");
        store
            .claim_stop_operation(
                command_id.as_str().to_owned(),
                hub.worker_generation(),
                blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json.clone(),
                intent,
            )
            .await
            .expect("unfinished receipt");
        drop(admission);
        drop(delegation);
        hub.shutdown().await.expect("first shutdown");
        drop(hub);
        store.close().await.expect("first close");
        store = SqliteStoreHandle::open(root.path()).await.expect("reopen");
        hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("new generation");
    }
    // Root All holds its root fence while the descendant's first drain is paused.
    // Named cancellation holds its fence through the asynchronous settlement tail.
    let paused_session = child.clone();
    let pause_scope = if scope == "named" { "named" } else { "tree" };
    let (arrived, resume) =
        crate::delegation::pause_after_first_drain(&paused_session, pause_scope);
    let session = if scope == "root" && !recovery {
        parent.clone()
    } else {
        child.clone()
    };
    let original = if scope == "root" && !recovery {
        old.clone()
    } else {
        child_run.clone()
    };
    let successor = RunId::new(format!("{label}-later"));
    if recovery {
        hub.append(&mut [run_state_envelope(
            &session,
            &original,
            hub.worker_generation(),
            &format!("{label}-old-done"),
            RunState::Done,
        )])
        .await
        .expect("original finished before recovery");
        hub.accept_turn(accept_command(
            &session,
            &successor,
            hub.worker_generation(),
            &format!("{label}-later"),
        ))
        .await
        .expect("successor before receipt recovery");
    }
    // Recovery must reuse the old intent even when the fresh proposed
    // snapshot already contains the successor's watch and pending report.
    let before_stop_report = if recovery {
        Some(successor_report(&hub, &session, &successor, &label, direct).await)
    } else {
        None
    };
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    if scope != "root" {
        connection
            .request(
                haider_rpc::RequestId::new("attach-parent"),
                haider_rpc::RequestBody::SessionAttach {
                    session_id: parent.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("control attachment to the parent");
    }
    let request = match scope {
        "named" => haider_rpc::RequestBody::AgentCancel {
            command_id,
            session_id: parent.clone(),
            worker_generation: hub.worker_generation(),
            agent,
        },
        "tree" => haider_rpc::RequestBody::AgentCancelAll {
            command_id,
            session_id: parent.clone(),
            worker_generation: hub.worker_generation(),
        },
        _ => haider_rpc::RequestBody::HeadlessRunStop {
            command_id,
            run_id: old,
            scope: haider_rpc::RunStopScopeWire::All,
        },
    };
    let mut stop = tokio::spawn(async move {
        let outcome = connection
            .request(haider_rpc::RequestId::new("stop"), request)
            .await;
        connection.close().await.expect("close control connection");
        outcome
    });
    tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, async {
        tokio::select! {
            reached = arrived => reached.expect("drain barrier"),
            early = &mut stop => panic!("stop returned before the first drain: {early:?}; frames: {:?}", sink.0.lock().expect("early frames")),
        }
    }).await.expect("stop reached first drain");
    if !recovery {
        hub.append(&mut [run_state_envelope(
            &session,
            &original,
            hub.worker_generation(),
            &format!("{label}-old-done"),
            RunState::Cancelled,
        )])
        .await
        .expect("original settles during stop");
        hub.accept_turn(accept_command(
            &session,
            &successor,
            hub.worker_generation(),
            &format!("{label}-later"),
        ))
        .await
        .expect("successor during settlement");
    }
    let report = match before_stop_report {
        Some(report) => report,
        None => successor_report(&hub, &session, &successor, &label, direct).await,
    };
    let monitor_id = report.monitor_id.clone();
    let before = hub
        .inner_monitor()
        .stop_fences()
        .epoch(&session, &monitor_id);
    let _delivery = crate::completion::wake_monitor(&hub, report.clone()).await;
    // With no WorkerManager installed the handoff can fail after durable admission;
    // the actual accepted user event is the evidence, never just a return code.
    let events = store.read(&session, 0, 4096).await.expect("journal");
    let admitted_runs = events
        .iter()
        .filter(|e| e.event_id.as_str().starts_with("monitor-user-"))
        .filter_map(|e| e.run_id.clone())
        .collect::<Vec<_>>();
    let admitted = !admitted_runs.is_empty();
    let lease = hub
        .acquire_worker_lease(session.clone())
        .await
        .expect("lease");
    let registration = hub
        .execute_monitor_tool(
            &lease,
            crate::monitor::MonitorToolCoordinates {
                run_id: successor.clone(),
                branch_id: None,
                agent_id: None,
                call_id: format!("{label}-tool-watch"),
                device_id: hub.device_id(),
                workspace_root: root.path().display().to_string(),
                approved_command: None,
                approved_file_path: None,
            },
            MonitorRequest::Register {
                source: MonitorSource::Timer {
                    interval_ms: 3_600_000,
                },
                filter: None,
                action: MonitorAction {
                    report: true,
                    follow_up: None,
                },
                occurrence: MonitorOccurrence::Every,
                lifetime: MonitorLifetime::Session,
            },
        )
        .await
        .expect("registration");
    drop(lease);
    // Complete the captured child so the stop tail cannot outlive the fixture.
    hub.append(&mut [run_state_envelope(
        &child,
        &child_run,
        hub.worker_generation(),
        &format!("{label}-helper-done"),
        RunState::Cancelled,
    )])
    .await
    .expect("captured helper settles");
    resume.send(()).expect("release stop");
    stop.await.expect("stop joins").expect("stop response");
    let frames = sink.0.lock().expect("stop frames").clone();
    assert!(
        frames.iter().any(|frame| matches!(
            (scope, frame),
            (
                "root",
                WireFrame::Response {
                    body: haider_rpc::ResponseBody::HeadlessRunStop { .. },
                    ..
                }
            ) | (
                "tree",
                WireFrame::Response {
                    body: haider_rpc::ResponseBody::AgentCancelAll { .. },
                    ..
                }
            ) | (
                "named",
                WireFrame::Response {
                    body: haider_rpc::ResponseBody::AgentCancel { .. },
                    ..
                }
            )
        )),
        "stop must commit its typed response: {frames:?}"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        while hub.inner_monitor().stop_fences().session_fenced(&child) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("named tail settles");
    assert_eq!(
        hub.inner_monitor()
            .stop_fences()
            .epoch(&session, &monitor_id),
        before,
        "successor watch was re-epoched"
    );
    let live = crate::delegation::DelegationHandle::new(hub.clone())
        .live_session_runs(&session)
        .await
        .expect("live runs");
    assert!(live.contains(&successor), "successor cancelled");
    for run in &admitted_runs {
        assert!(
            live.contains(run),
            "older stop cancelled the successor report's accepted follow-up"
        );
    }
    let dismissed = crate::completion::load(&hub, &session)
        .await
        .expect("settled completion journal")
        .events
        .iter()
        .any(|e| {
            matches!(e,
        haider_protocol::completion::CompletionEvent::CompletionDismissed {obligation_id,..}
            if obligation_id == &format!("monitor:{}", report.report_id))
        });
    let listed = hub.inner_monitor().client_list(&hub, session.clone()).await;
    assert!(
        matches!(listed.outcome,haider_rpc::MonitorListOutcomeWire::Listed {monitors} if monitors.iter().any(|m|m.monitor_id==monitor_id)),
        "successor watch removed"
    );
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen final journal");
    let persisted = reopened
        .read(&session, 0, 4096)
        .await
        .expect("persisted report truth");
    let persisted_dismissed = persisted.iter().any(|e| {
        matches!(e.payload.decode::<haider_protocol::completion::CompletionEvent>(),
        Ok(haider_protocol::completion::CompletionEvent::CompletionDismissed {obligation_id,..})
            if obligation_id == format!("monitor:{}", report.report_id))
    });
    reopened.close().await.expect("close reopened store");
    assert_eq!(
        dismissed, persisted_dismissed,
        "cached and reopened report truth must agree"
    );
    assert!(
        !dismissed,
        "older stop dismissed the successor report ({scope}, recovery={recovery}, direct={direct})"
    );
    assert!(admitted, "successor report never reached durable admission");
    assert_eq!(
        registration.status,
        haider_protocol::tool::ToolResultStatus::Completed,
        "successor registration refused: {}",
        registration.preview
    );
}

#[tokio::test]
async fn r7_successor_root_settlement_pending() {
    successor_during_stop("root", false, false).await;
}

#[tokio::test]
async fn r7_successor_root_settlement_direct() {
    successor_during_stop("root", false, true).await;
}

#[tokio::test]
async fn r7_successor_root_receipt_recovery_pending() {
    successor_during_stop("root", true, false).await;
}

#[tokio::test]
async fn r7_successor_root_receipt_recovery_direct() {
    successor_during_stop("root", true, true).await;
}

#[tokio::test]
async fn r7_successor_tree_settlement_pending() {
    successor_during_stop("tree", false, false).await;
}

#[tokio::test]
async fn r7_successor_tree_settlement_direct() {
    successor_during_stop("tree", false, true).await;
}

#[tokio::test]
async fn r7_successor_tree_receipt_recovery_pending() {
    successor_during_stop("tree", true, false).await;
}

#[tokio::test]
async fn r7_successor_tree_receipt_recovery_direct() {
    successor_during_stop("tree", true, true).await;
}

#[tokio::test]
async fn r7_successor_named_settlement_pending() {
    successor_during_stop("named", false, false).await;
}

#[tokio::test]
async fn r7_successor_named_settlement_direct() {
    successor_during_stop("named", false, true).await;
}

#[tokio::test]
async fn r7_successor_named_receipt_recovery_pending() {
    successor_during_stop("named", true, false).await;
}

#[tokio::test]
async fn r7_successor_named_receipt_recovery_direct() {
    successor_during_stop("named", true, true).await;
}
