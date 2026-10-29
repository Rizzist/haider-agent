#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::session_hub_private_tests::{
    CapturingFrameSink, accept_command, create_command, run_state_envelope,
};
use super::*;
use haider_core::{DelegationRecord, DelegationState};
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::headless::{HeadlessRunEventPayload, HeadlessRunSpecV1, RunBudgetV1};
use haider_protocol::ids::{AgentId, ItemId, LeaseId};
use haider_tools::{
    MonitorAction, MonitorLifetime, MonitorOccurrence, MonitorRequest, MonitorSource,
};
use std::time::Duration;

async fn add_grandchild_watch(
    hub: &SessionHub,
    original: &DelegationRecord,
    root: &std::path::Path,
    label: &str,
    direct: bool,
) -> (DelegationRecord, crate::monitor::MonitorReport) {
    let mut grandchild = original.clone();
    grandchild.agent_id = AgentId::new(format!("{label}-grandchild"));
    grandchild.child_session_id = SessionId::new(format!("{label}-grandchild-session"));
    grandchild.child_run_id = RunId::new(format!("{label}-grandchild-run"));
    grandchild.parent_session_id = original.child_session_id.clone();
    grandchild.parent_run_id = original.child_run_id.clone();
    grandchild.parent_agent_id = Some(original.agent_id.clone());
    grandchild.depth = original.depth + 1;
    grandchild.call_id = format!("{label}-grandchild-spawn");
    grandchild.tool_item_id = ItemId::new(format!("{label}-grandchild-item"));
    grandchild.manifest.agent = grandchild.agent_id.clone();
    grandchild.manifest.parent = Some(original.agent_id.clone());
    grandchild.manifest.lease = LeaseId::new(format!("{label}-grandchild-lease"));
    hub.create_internal_session(create_command(
        &grandchild.child_session_id,
        &format!("{label}-grandchild"),
    ))
    .await
    .expect("grandchild session");
    hub.create_delegation(grandchild.clone())
        .await
        .expect("grandchild delegation");
    hub.accept_turn(accept_command(
        &grandchild.child_session_id,
        &grandchild.child_run_id,
        hub.worker_generation(),
        &format!("{label}-grandchild"),
    ))
    .await
    .expect("grandchild accepted");
    let coordinator = crate::delegation::DelegationHandle::new(hub.clone());
    assert!(
        !coordinator
            .parent_run_stopping(&grandchild.agent_id)
            .await
            .expect("actual launch check"),
        "grandchild must be launchable before its parent is cancelled"
    );
    hub.mark_delegation_running(grandchild.agent_id.clone())
        .await
        .expect("grandchild running");
    let lease = hub
        .acquire_worker_lease(grandchild.child_session_id.clone())
        .await
        .expect("grandchild lease");
    let result = hub
        .execute_monitor_tool(
            &lease,
            crate::monitor::MonitorToolCoordinates {
                run_id: grandchild.child_run_id.clone(),
                branch_id: None,
                agent_id: Some(grandchild.agent_id.clone()),
                call_id: format!("{label}-original-grandchild-watch"),
                device_id: hub.device_id(),
                workspace_root: root.display().to_string(),
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
                    follow_up: Some("original grandchild follow-up".into()),
                },
                occurrence: MonitorOccurrence::Every,
                lifetime: MonitorLifetime::Session,
            },
        )
        .await
        .expect("grandchild tool registration");
    drop(lease);
    assert_eq!(
        result.status,
        haider_protocol::tool::ToolResultStatus::Completed,
        "original grandchild registration must be accepted"
    );
    let listed = hub
        .inner_monitor()
        .client_list(hub, grandchild.child_session_id.clone())
        .await;
    let monitor_id = match listed.outcome {
        haider_rpc::MonitorListOutcomeWire::Listed { monitors } => {
            assert_eq!(monitors.len(), 1);
            monitors[0].monitor_id.clone()
        }
        other => panic!("grandchild listing: {other:?}"),
    };
    let report = crate::monitor::MonitorReport {
        report_id: format!("{label}-grandchild-report"),
        monitor_id,
        session_id: grandchild.child_session_id.clone(),
        branch_id: None,
        agent_id: Some(grandchild.agent_id.clone()),
        source: haider_tools::MonitorSourceKind::Timer,
        occurrence: MonitorOccurrence::Every,
        status: crate::monitor::MonitorReportStatus::Matched,
        events: vec![],
        coalesced_count: 1,
        omitted_count: 0,
        action: MonitorAction {
            report: true,
            follow_up: Some("original grandchild follow-up".into()),
        },
    };
    if !direct {
        let mut pending = run_state_envelope(
            &grandchild.child_session_id,
            &grandchild.child_run_id,
            hub.worker_generation(),
            &format!("{label}-grandchild-pending"),
            RunState::Queued,
        );
        *pending.payload = serde_json::json!({"type":"monitor_report_pending", "pending":{"report":report,"queue_order":1,"queued_at_ms":1}});
        hub.append(&mut [pending])
            .await
            .expect("grandchild pending report");
    }
    (grandchild, report)
}

async fn check_variant(scope: &str, direct: bool, known_before_claim: bool, variant: &str) {
    let label = format!("r8-late-{scope}-{direct}-{known_before_claim}-{variant}");
    let root = tempfile::tempdir().expect("profile");
    let mut store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let mut manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("test worker manager");
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
                tools: vec!["spawn_subagent".into(), "monitor".into()],
                effect_ceiling: vec![haider_protocol::effect::EffectClass::AgentSpawn],
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
    if variant == "recovery" {
        let delegation = crate::delegation::DelegationHandle::new(hub.clone());
        let monitor = hub.inner_monitor();
        let admission = monitor.admission_lock().await;
        let tree = if scope == "tree" {
            crate::delegation::SubagentTreeRoot::Session(parent.clone())
        } else {
            crate::delegation::SubagentTreeRoot::Run {
                session_id: parent.clone(),
                run_id: old.clone(),
            }
        };
        let intent = delegation
            .stop_incarnations(&tree, &admission)
            .await
            .expect("initial claim targets");
        let request = if scope == "tree" {
            serde_json::json!({"operation":"agent.cancel_all", "session_id":parent})
        } else {
            serde_json::json!({"operation":"headless.run.stop", "session_id":parent, "run_id":old, "scope":if scope == "all" { haider_rpc::RunStopScopeWire::All } else { haider_rpc::RunStopScopeWire::Subagents }})
        };
        let request_json = serde_json::to_string(&request).expect("request json");
        store
            .claim_stop_operation(
                command_id.as_str().to_owned(),
                hub.worker_generation(),
                blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json,
                serde_json::to_value(intent).expect("intent"),
            )
            .await
            .expect("unfinished receipt");
        drop(admission);
    }
    let before = if known_before_claim || variant == "recovery" {
        Some(
            add_grandchild_watch(
                &hub,
                &record,
                root.path(),
                &label,
                direct || variant == "recovery",
            )
            .await,
        )
    } else {
        None
    };
    if variant == "recovery" {
        manager.shutdown().await.expect("first manager shutdown");
        hub.shutdown().await.expect("first hub shutdown");
        // Seed the cold snapshot's pending report after retiring the old
        // delivery service, so it cannot consume the restart fixture early.
        let (_, report) = before.as_ref().expect("recovered helper");
        let mut pending = run_state_envelope(
            &report.session_id,
            &RunId::new("pending-fixture"),
            hub.worker_generation(),
            &format!("{label}-cold-pending"),
            RunState::Queued,
        );
        pending.run_id = None;
        *pending.payload = serde_json::json!({"type":"monitor_report_pending", "pending":{"report":report,"queue_order":1,"queued_at_ms":1}});
        store
            .append(&mut [pending])
            .await
            .expect("cold pending report snapshot");
        store.close().await.expect("first store close");
        store = SqliteStoreHandle::open(root.path()).await.expect("reopen");
        hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("restarted hub");
        manager = crate::worker::WorkerManager::start(
            hub.clone(),
            crate::worker::WorkerDependencies::unconfigured_for_tests(),
            false,
        );
    }
    let recovery_monitor = hub.inner_monitor();
    let recovery_delivery = if variant == "recovery" {
        Some(recovery_monitor.completion_lock().await)
    } else {
        None
    };
    if variant == "recovery" {
        hub.install_worker_manager(manager.handle())
            .expect("restarted manager");
    }
    let recovered_state = if variant == "recovery" {
        let (record, _) = before.as_ref().expect("recovered helper");
        let events = store
            .read(&record.child_session_id, 0, 4096)
            .await
            .expect("pre-stop recovered state");
        let state = events.iter().rev().find_map(|event| {
            if event.run_id.as_ref() != Some(&record.child_run_id) {
                return None;
            }
            match event.payload.decode_event() {
                Ok(EventPayload::RunState(state)) => Some(state),
                _ => None,
            }
        });
        assert!(
            state.as_ref().is_some_and(|state| state.is_terminal()),
            "retired service must record the original task's terminal state"
        );
        state
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
        .expect("parent attachment");
    let request = match scope {
        "tree" => haider_rpc::RequestBody::AgentCancelAll {
            command_id,
            session_id: parent.clone(),
            worker_generation: hub.worker_generation(),
        },
        _ => haider_rpc::RequestBody::HeadlessRunStop {
            command_id,
            run_id: old.clone(),
            scope: if scope == "all" {
                haider_rpc::RunStopScopeWire::All
            } else {
                haider_rpc::RunStopScopeWire::Subagents
            },
        },
    };
    let (arrived, resume) = crate::delegation::pause_after_first_drain(&child, "tree");
    let replay_request = request.clone();
    let mut stop = tokio::spawn(async move {
        let outcome = connection
            .request(haider_rpc::RequestId::new("stop"), request)
            .await;
        connection.close().await.expect("close stop connection");
        outcome
    });
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::select! {
            reached = arrived => reached.expect("first child drain"),
            early = &mut stop => panic!("stop finished before child drain: {early:?}"),
        }
    })
    .await
    .expect("first child is paused before cancellation");
    let (mut grandchild, mut report) = match before {
        Some(pair) => pair,
        None => add_grandchild_watch(&hub, &record, root.path(), &label, direct).await,
    };
    if variant == "deep" {
        for depth in 3..=5 {
            (grandchild, report) = add_grandchild_watch(
                &hub,
                &grandchild,
                root.path(),
                &format!("{label}-depth-{depth}"),
                direct,
            )
            .await;
        }
    }
    if variant == "updated" {
        let lease = hub
            .acquire_worker_lease(grandchild.child_session_id.clone())
            .await
            .expect("update lease");
        let result = hub
            .execute_monitor_tool(
                &lease,
                crate::monitor::MonitorToolCoordinates {
                    run_id: RunId::new(format!("{label}-updater")),
                    branch_id: None,
                    agent_id: Some(grandchild.agent_id.clone()),
                    call_id: format!("{label}-update"),
                    device_id: hub.device_id(),
                    workspace_root: root.path().display().to_string(),
                    approved_command: None,
                    approved_file_path: None,
                },
                MonitorRequest::Update {
                    monitor_id: report.monitor_id.clone(),
                    source: MonitorSource::Timer {
                        interval_ms: 3_600_000,
                    },
                    filter: None,
                    action: report.action.clone(),
                    occurrence: MonitorOccurrence::Every,
                    lifetime: MonitorLifetime::Session,
                },
            )
            .await
            .expect("update");
        assert_eq!(
            result.status,
            haider_protocol::tool::ToolResultStatus::Completed
        );
    }
    let independent = if variant == "independent" {
        let receipt = hub
            .inner_monitor()
            .client_register(
                &hub,
                crate::monitor::MonitorClientRegistrationRequest {
                    command_id: haider_rpc::CommandId::new(format!("{label}-independent")),
                    session_id: grandchild.child_session_id.clone(),
                    worker_generation: hub.worker_generation(),
                    source: haider_rpc::MonitorSourceWire::Timer {
                        interval_ms: 3_600_000,
                    },
                    filter: None,
                    action: haider_rpc::MonitorActionWire {
                        report: true,
                        follow_up: None,
                    },
                    occurrence: haider_rpc::MonitorOccurrenceWire::Every,
                    lifetime: haider_rpc::MonitorLifetimeWire::Session,
                },
            )
            .await;
        match receipt.outcome {
            haider_rpc::MonitorRegisterOutcomeWire::Registered { monitor } => {
                Some(monitor.monitor_id)
            }
            other => panic!("independent watch: {other:?}"),
        }
    } else {
        None
    };
    let mut direct_admitted = false;
    if variant == "direct-inflight" {
        let (discovered, finish) =
            crate::delegation::pause_after_first_drain(&grandchild.child_session_id, "tree");
        resume.send(()).expect("release first child stop");
        tokio::time::timeout(Duration::from_secs(20), discovered)
            .await
            .expect("discovered helper drain")
            .expect("discovered barrier");
        crate::completion::wake_monitor(&hub, report.clone())
            .await
            .expect("fenced direct report");
        let events = store
            .read(&grandchild.child_session_id, 0, 4096)
            .await
            .expect("fenced direct journal");
        direct_admitted = events
            .iter()
            .any(|e| e.event_id.as_str().starts_with("monitor-user-"));
        finish.send(()).expect("release discovered helper");
    } else {
        resume.send(()).expect("release first child stop");
    }
    tokio::time::timeout(Duration::from_secs(20), stop)
        .await
        .expect("stop settles")
        .expect("stop join")
        .expect("stop request");
    drop(recovery_delivery);
    let frames = sink.0.lock().expect("frames").clone();
    for frame in &frames {
        if matches!(
            frame,
            WireFrame::Response {
                body: haider_rpc::ResponseBody::AgentCancelAll { .. }
                    | haider_rpc::ResponseBody::HeadlessRunStop { .. },
                ..
            }
        ) {
            eprintln!("R8_LATE_STOP_RESPONSE {frame:?}");
        }
    }
    assert!(
        frames.iter().any(|f| matches!(
            f,
            WireFrame::Response {
                body: haider_rpc::ResponseBody::AgentCancelAll { .. }
                    | haider_rpc::ResponseBody::HeadlessRunStop { .. },
                ..
            }
        )),
        "typed stop receipt required: {frames:?}"
    );
    let coordinator = crate::delegation::DelegationHandle::new(hub.clone());
    let grandchild_stopped = !coordinator
        .live_session_runs(&grandchild.child_session_id)
        .await
        .expect("live grandchild runs")
        .contains(&grandchild.child_run_id);
    let state_events = store
        .read(&grandchild.child_session_id, 0, 4096)
        .await
        .expect("grandchild terminal journal");
    let terminal_state = state_events.iter().rev().find_map(|e| {
        if e.run_id.as_ref() != Some(&grandchild.child_run_id) {
            return None;
        }
        match e.payload.decode_event() {
            Ok(EventPayload::RunState(state)) => Some(state),
            _ => None,
        }
    });
    let listed = hub
        .inner_monitor()
        .client_list(&hub, grandchild.child_session_id.clone())
        .await;
    let retained = match listed.outcome {
        haider_rpc::MonitorListOutcomeWire::Listed { monitors } => {
            if let Some(id) = independent {
                assert!(
                    monitors.iter().any(|m| m.monitor_id == id),
                    "independent client watch must survive discovery"
                );
            }
            monitors.iter().any(|m| m.monitor_id == report.monitor_id)
        }
        other => panic!("final grandchild listing: {other:?}"),
    };
    let journal = crate::completion::load(&hub, &grandchild.child_session_id)
        .await
        .expect("completion truth");
    let settled = journal
        .projection
        .is_settled(&format!("monitor:{}", report.report_id));
    // A retained registration can still produce a report. Exercise the real
    // delivery seam and retain actual journal admission, not a returned flag.
    let before_events = store
        .read(&grandchild.child_session_id, 0, 4096)
        .await
        .expect("before delivery journal");
    assert!(
        !before_events
            .iter()
            .any(|e| e.event_id.as_str().starts_with("monitor-user-")),
        "probe must not pre-admit its report"
    );
    if !direct {
        let _ = crate::completion::wake_monitor(&hub, report.clone()).await;
    }
    let events = store
        .read(&grandchild.child_session_id, 0, 4096)
        .await
        .expect("post-stop journal");
    let admitted = events
        .iter()
        .any(|e| e.event_id.as_str().starts_with("monitor-user-"));
    let registered_run = events
        .iter()
        .find(|e| e.event_id.as_str().starts_with("monitor-registered-"))
        .and_then(|e| e.run_id.as_ref())
        .map(|r| r.as_str().to_owned());
    eprintln!(
        "R8_LATE_DESCENDANT scope={scope} direct={direct} captured_control={known_before_claim} grandchild_stopped={grandchild_stopped} watch_retained={retained} pending_settled={settled} followup_admitted={admitted} registration_run={registered_run:?} terminal_state={terminal_state:?}"
    );
    tokio::time::timeout(Duration::from_secs(20), manager.shutdown())
        .await
        .expect("bounded manager shutdown")
        .expect("manager shutdown");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close store");
    if variant == "recovery" {
        let reopened = SqliteStoreHandle::open(root.path())
            .await
            .expect("reopen completed stop");
        let restarted = SessionHub::new(reopened.clone(), SessionHubConfig::default())
            .expect("completed restart");
        let listed = restarted
            .inner_monitor()
            .client_list(&restarted, grandchild.child_session_id.clone())
            .await;
        assert!(
            matches!(listed.outcome, haider_rpc::MonitorListOutcomeWire::Listed { monitors } if !monitors.iter().any(|m|m.monitor_id==report.monitor_id)),
            "stopped original watch restored after restart"
        );
        assert!(
            crate::completion::load(&restarted, &grandchild.child_session_id)
                .await
                .expect("reopened completion")
                .projection
                .is_settled(&format!("monitor:{}", report.report_id))
        );
        let before = reopened
            .latest_seq(&grandchild.child_session_id)
            .await
            .expect("before replay");
        let replay_sink = Arc::new(CapturingFrameSink::default());
        let replay = restarted
            .open_connection(
                std::collections::BTreeSet::from([
                    haider_rpc::Capability::View,
                    haider_rpc::Capability::Control,
                ]),
                replay_sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("replay connection");
        replay
            .request(
                haider_rpc::RequestId::new("replay-attach"),
                haider_rpc::RequestBody::SessionAttach {
                    session_id: parent.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("replay attach");
        replay
            .request(haider_rpc::RequestId::new("replay"), replay_request)
            .await
            .expect("completed receipt replay");
        let response = |frames: &[WireFrame]| {
            frames.iter().find_map(|f| match f {
                WireFrame::Response {
                    body:
                        body @ (haider_rpc::ResponseBody::AgentCancelAll { .. }
                        | haider_rpc::ResponseBody::HeadlessRunStop { .. }),
                    ..
                } => Some(body.clone()),
                _ => None,
            })
        };
        assert_eq!(
            response(&replay_sink.0.lock().expect("replay frames")),
            response(&frames),
            "completed receipt must replay its original response"
        );
        assert_eq!(
            reopened
                .latest_seq(&grandchild.child_session_id)
                .await
                .expect("after replay"),
            before,
            "completed receipt replay changed descendant journal"
        );
        replay.close().await.expect("close replay");
        restarted
            .shutdown()
            .await
            .expect("completed restart shutdown");
        reopened.close().await.expect("completed restart close");
    }
    assert!(
        grandchild_stopped,
        "the late grandchild run must be part of the stopped tree"
    );
    if variant == "recovery" {
        assert_eq!(
            terminal_state, recovered_state,
            "receipt recovery must retain the original task terminal truth"
        );
    } else {
        assert_eq!(
            terminal_state,
            Some(RunState::Cancelled),
            "the real stop must cancel the original grandchild run"
        );
    }
    assert!(
        !retained,
        "a stopped late grandchild retained its original tool watch"
    );
    assert!(
        !direct_admitted,
        "direct report admitted during discovered helper settlement"
    );
    // Direct reports not previously observed have no durable obligation until
    // delivery; the registry-removal assertion above is their ownership check.
    if !direct {
        assert!(
            settled && !admitted,
            "a stopped late grandchild's pending report resumed work"
        );
    }
}

async fn check(scope: &str, direct: bool, known_before_claim: bool) {
    check_variant(scope, direct, known_before_claim, "plain").await;
}

#[tokio::test]
async fn r8_late_descendant_all_pending() {
    check("all", false, false).await;
}

#[tokio::test]
async fn r8_late_descendant_all_direct() {
    check("all", true, false).await;
}

#[tokio::test]
async fn r8_captured_descendant_all_control() {
    check("all", false, true).await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_pending() {
    check("subagents", false, false).await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_direct() {
    check("subagents", true, false).await;
}

#[tokio::test]
async fn r8_captured_descendant_subagents_control() {
    check("subagents", false, true).await;
}

#[tokio::test]
async fn r8_late_descendant_tree_pending() {
    check("tree", false, false).await;
}

#[tokio::test]
async fn r8_late_descendant_tree_direct() {
    check("tree", true, false).await;
}

#[tokio::test]
async fn r8_captured_descendant_tree_control() {
    check("tree", false, true).await;
}

#[tokio::test]
async fn r8_late_descendant_all_deep() {
    check_variant("all", false, false, "deep").await;
}

#[tokio::test]
async fn r8_late_descendant_all_updated() {
    check_variant("all", false, false, "updated").await;
}

#[tokio::test]
async fn r8_late_descendant_all_recovery() {
    check_variant("all", false, false, "recovery").await;
}

#[tokio::test]
async fn r8_late_descendant_all_independent() {
    check_variant("all", false, false, "independent").await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_deep() {
    check_variant("subagents", false, false, "deep").await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_updated() {
    check_variant("subagents", false, false, "updated").await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_recovery() {
    check_variant("subagents", false, false, "recovery").await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_independent() {
    check_variant("subagents", false, false, "independent").await;
}

#[tokio::test]
async fn r8_late_descendant_tree_deep() {
    check_variant("tree", false, false, "deep").await;
}

#[tokio::test]
async fn r8_late_descendant_tree_updated() {
    check_variant("tree", false, false, "updated").await;
}

#[tokio::test]
async fn r8_late_descendant_tree_recovery() {
    check_variant("tree", false, false, "recovery").await;
}

#[tokio::test]
async fn r8_late_descendant_tree_independent() {
    check_variant("tree", false, false, "independent").await;
}

#[tokio::test]
async fn r8_late_descendant_all_direct_inflight() {
    check_variant("all", true, false, "direct-inflight").await;
}

#[tokio::test]
async fn r8_late_descendant_subagents_direct_inflight() {
    check_variant("subagents", true, false, "direct-inflight").await;
}

#[tokio::test]
async fn r8_late_descendant_tree_direct_inflight() {
    check_variant("tree", true, false, "direct-inflight").await;
}
