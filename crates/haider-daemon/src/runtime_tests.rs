#![allow(clippy::expect_used)]

//! Unit tests for the drain barrier's arbitration helpers.
//!
//! These are crate-internal on purpose: `bounded_finalization` and
//! `barrier_breached` decide whether a shutdown may call itself graceful, and
//! the interesting cases (a step that completes exactly as the deadline
//! passes, a second signal that arrives during a step) are driven far more
//! precisely with synthetic futures than with a real daemon.

use super::*;
use std::future::pending;
use std::time::Duration;
use tokio::sync::watch;

fn shutdown_channel() -> (
    watch::Sender<ShutdownRequest>,
    watch::Receiver<ShutdownRequest>,
) {
    watch::channel(ShutdownRequest::Graceful {
        reason: "test drain".into(),
    })
}

/// The shared startup scan feeds turn recovery, hooks, and the native sidecar
/// before a session hub exists. Recovery-appended terminal facts must be
/// caught up by that same session fold; no later append may hide the ordering
/// hole this test pins.
#[tokio::test]
async fn startup_recovery_run_failed_reaches_sidecar_without_later_append() {
    use crate::SessionHubConfig;
    use crate::turn_recovery::recover_interrupted_turns_report_with_visitor;
    use haider_core::{SqliteStoreHandle, StoreHandle};
    use haider_protocol::EventPayload;
    use haider_protocol::envelope::{
        EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
    };
    use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
    use haider_protocol::pipe::sidecar_row_line;
    use haider_protocol::state::RunState;

    let root = tempfile::tempdir().expect("profile");
    let first = SqliteStoreHandle::open(root.path())
        .await
        .expect("open first generation");
    let session_id = SessionId::new("startup-recovery-sidecar");
    let run_id = RunId::new("interrupted-run");
    let device_id = DeviceId::new("old-daemon");
    let mut interrupted = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("interrupted-running"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id),
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: first.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Thinking))
            .expect("running payload"),
    }];
    StoreHandle::append(&first, &mut interrupted)
        .await
        .expect("seed interrupted run");
    first.close().await.expect("close first generation");

    let recovered = SqliteStoreHandle::open(root.path())
        .await
        .expect("open recovery generation");
    let mut hydration = StartupHydration::prepare(&recovered)
        .await
        .expect("prepare shared hydration");
    let recovery = recover_interrupted_turns_report_with_visitor(
        &recovered,
        &DeviceId::new("new-daemon"),
        &mut hydration,
    )
    .await
    .expect("terminalize interrupted run");
    assert_eq!(recovery.touched_sessions, vec![session_id.clone()]);
    let events = StoreHandle::read(&recovered, &session_id, 0, 64)
        .await
        .expect("read recovered journal");
    let failed: &RawEnvelope = events
        .iter()
        .find(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::RunFailed { .. })
            )
        })
        .expect("recovery committed RunFailed");
    let expected_error_row = sidecar_row_line(failed).expect("RunFailed projects to sidecar");

    let (hook_hydration, pipe_native) = hydration.into_parts();
    assert_eq!(
        hook_hydration.scan_start(&session_id),
        events.last().map_or(0, |envelope| envelope.seq),
        "hook reducer must catch up through the recovery-appended suffix"
    );
    let hub = SessionHub::new_with_pipe_native(
        recovered.clone(),
        SessionHubConfig::default(),
        pipe_native,
    )
    .expect("hub");
    let sidecar =
        std::fs::read_to_string(root.path().join("pipe").join(format!("{session_id}.pipe")))
            .expect("startup reconcile creates sidecar");
    assert!(
        sidecar.lines().any(|line| line == expected_error_row),
        "recovery error row must be present without a subsequent append"
    );
    hub.shutdown().await.expect("hub shutdown");
    recovered.close().await.expect("store close");
}

#[tokio::test]
async fn shared_startup_hydration_is_page_bounded_across_sessions() {
    use crate::turn_recovery::recover_interrupted_turns_report_with_visitor;
    use haider_core::{SqliteStoreHandle, StoreHandle};
    use haider_protocol::EventPayload;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
    use haider_protocol::state::RunState;

    const EVENTS_PER_SESSION: usize = 513;
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut sessions = Vec::new();
    for name in ["shared-hydration-alpha", "shared-hydration-beta"] {
        let session_id = SessionId::new(name);
        let mut events = (0..EVENTS_PER_SESSION)
            .map(|index| EventEnvelope {
                schema_version: SCHEMA_VERSION,
                event_id: EventId::new(format!("{name}-{index}")),
                seq: 0,
                session_id: session_id.clone(),
                branch_id: None,
                run_id: Some(RunId::new(format!("{name}-run-{index}"))),
                agent_id: None,
                device_id: DeviceId::new("shared-hydration-device"),
                authority_epoch: 0,
                worker_generation: store.worker_generation(),
                causation_id: None,
                correlation_id: None,
                committed_at_ms: 0,
                render: RenderTargets {
                    ui: true,
                    durable: true,
                    prompt: PromptRender::Omit,
                },
                payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
                    .expect("terminal payload"),
            })
            .collect::<Vec<_>>();
        StoreHandle::append(&store, &mut events)
            .await
            .expect("append page-crossing journal");
        sessions.push(session_id);
    }

    let mut hydration = StartupHydration::prepare(&store)
        .await
        .expect("prepare shared hydration");
    recover_interrupted_turns_report_with_visitor(
        &store,
        &DeviceId::new("shared-hydration-recovery"),
        &mut hydration,
    )
    .await
    .expect("page-fed shared hydration");
    let (hooks, _pipe_native) = hydration.into_parts();
    for session_id in sessions {
        assert_eq!(
            hooks.scan_start(&session_id),
            u64::try_from(EVENTS_PER_SESSION).expect("small event count")
        );
        let sidecar =
            std::fs::read_to_string(root.path().join("pipe").join(format!("{session_id}.pipe")))
                .expect("page-fed sidecar exists");
        assert!(sidecar.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| value.get("coverage").and_then(serde_json::Value::as_u64))
                == Some(u64::try_from(EVENTS_PER_SESSION).expect("small event count"))
        }));
    }
    store.close().await.expect("store close");
}

#[tokio::test]
async fn a_step_that_completes_is_not_evidence_that_the_barrier_held() {
    let (_sender, shutdown) = shutdown_channel();
    let expired = tokio::time::Instant::now() - Duration::from_secs(1);
    let mut receiver = shutdown.clone();

    // The work is ready on the very first poll — the step DID complete, and its
    // result must be returned rather than thrown away...
    let completed = bounded_finalization(std::future::ready(7_u8), expired, &mut receiver).await;
    assert_eq!(completed, Some(7));
    // ...but the caller's arbitration is what decides the outcome, and it must
    // see the breach that the completed step said nothing about.
    assert!(
        barrier_breached(expired, &shutdown),
        "an expired deadline must be visible to post-step arbitration"
    );
}

#[tokio::test]
async fn a_pending_step_stops_at_the_deadline() {
    let (_sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
    let abandoned = bounded_finalization(pending::<()>(), deadline, &mut shutdown).await;
    assert!(
        abandoned.is_none(),
        "the barrier deadline must end the wait"
    );
}

#[tokio::test]
async fn a_second_signal_during_a_step_forces_the_outcome() {
    let (sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    tokio::spawn(async move {
        sender.send_replace(ShutdownRequest::Forced {
            reason: "second signal".into(),
        });
        // Hold the sender so the channel stays open: this is a force, not a
        // dropped controller.
        std::future::pending::<()>().await;
    });

    let abandoned = bounded_finalization(pending::<()>(), deadline, &mut shutdown).await;
    assert!(
        abandoned.is_none(),
        "a force arriving mid-step must abandon the wait well before the deadline"
    );
}

/// MUTATION CHECK: drop the force arm from `barrier_breached` (leave only the
/// deadline comparison). Expected failure: the force delivered without any
/// `changed()` poll goes unseen and this assertion fails. Verified 2026-07-27.
#[tokio::test]
async fn a_force_that_arrives_during_a_synchronous_step_is_still_observed() {
    let (sender, shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    assert!(!barrier_breached(deadline, &shutdown));

    // Nothing polls `changed()` here — this is the synchronous-step case (the
    // endpoint cleanup): arbitration must read the watch VALUE.
    sender.send_replace(ShutdownRequest::Forced {
        reason: "during cleanup".into(),
    });
    assert!(
        barrier_breached(deadline, &shutdown),
        "a force delivered while a synchronous step ran must still be seen"
    );
}

#[tokio::test]
async fn a_dropped_controller_is_not_a_second_signal() {
    let (sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    drop(sender);

    // Losing the ability to receive a force is not a force: the step keeps its
    // deadline, and work that finishes inside it still counts as completed.
    let completed = bounded_finalization(
        async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            9_u8
        },
        deadline,
        &mut shutdown,
    )
    .await;
    assert_eq!(completed, Some(9));
}

/// MUTATION CHECK: remove the Waiting(LocalChild) recovery arm or its
/// delegation/tool checkpoint reconstruction. Expected runtime failure: the
/// parent is terminalized as Errored and no `RecoveredWork::ChildWait` is
/// returned after the child has already committed Done.
#[tokio::test]
async fn child_done_parent_wait_crash_recovers_the_same_logical_turn() {
    use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
    use haider_core::{
        DelegationRecord, DelegationState, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
        TurnAcceptCommand,
    };
    use haider_protocol::DeliveryMode;
    use haider_protocol::EventPayload;
    use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{AgentId, DeviceId, EventId, ItemId, LeaseId, RunId, SessionId};
    use haider_protocol::item::{ItemDelta, ItemEvent, ToolStatus, TurnItem};
    use haider_protocol::state::{RunState, WaitReason};

    fn create_command(session_id: &SessionId, device_id: &DeviceId) -> SessionCreateCommand {
        SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("digest-{session_id}"),
            request_json: format!(r#"{{"session":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: device_id.clone(),
        }
    }

    fn envelope(
        generation: u64,
        device_id: &DeviceId,
        session_id: &SessionId,
        run_id: &RunId,
        suffix: &str,
        payload: EventPayload,
    ) -> haider_protocol::envelope::RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("crash-{suffix}")),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(run_id.clone()),
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
            payload: serde_json::to_value(payload).expect("payload"),
        }
    }

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open generation one");
    let generation = store.worker_generation();
    let device_id = DeviceId::new("recovery-test-device");
    let parent_session = SessionId::new("recovery-parent");
    let child_session = SessionId::new("recovery-child");
    let parent_run = RunId::new("recovery-parent-run");
    let child_run = RunId::new("recovery-child-run");
    let agent = AgentId::new("recovery-agent");
    let tool_item = ItemId::new("recovery-tool-item");
    store
        .create_session(create_command(&parent_session, &device_id))
        .await
        .expect("create parent");
    store
        .create_session(create_command(&child_session, &device_id))
        .await
        .expect("create child");
    store
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-parent".into(),
            request_digest: "accept-parent-digest".into(),
            request_json: r#"{"turn":"parent"}"#.into(),
            session_id: parent_session.clone(),
            worker_generation: generation,
            run_id: parent_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "delegate".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("parent-queued"),
            user_event_id: EventId::new("parent-user"),
            active_event_id: EventId::new("parent-active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept parent");
    store
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-child".into(),
            request_digest: "accept-child-digest".into(),
            request_json: r#"{"turn":"child"}"#.into(),
            session_id: child_session.clone(),
            worker_generation: generation,
            run_id: child_run.clone(),
            agent_id: Some(agent.clone()),
            branch_id: None,
            text: "child task".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("child-queued"),
            user_event_id: EventId::new("child-user"),
            active_event_id: EventId::new("child-active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept child");
    let manifest = AgentManifest {
        agent: agent.clone(),
        role: AgentRole::Subagent,
        task: "tests".into(),
        callsign: Some("SUB-RECOVERY".into()),
        model_profile: "fake-model".into(),
        grant: Grant {
            tools: Vec::new(),
            effect_ceiling: Vec::new(),
        },
        budget_tokens: Some(4096),
        placement: Placement::Local,
        lease: LeaseId::new("recovery-lease"),
        fencing_epoch: generation,
        attempt: 0,
        parent: None,
        coordinates: None,
        cli_scope: None,
    };
    store
        .create_delegation(DelegationRecord {
            agent_id: agent.clone(),
            child_session_id: child_session.clone(),
            child_run_id: child_run.clone(),
            parent_session_id: parent_session.clone(),
            parent_run_id: parent_run.clone(),
            parent_branch_id: None,
            call_id: "spawn-call".into(),
            tool_item_id: tool_item.clone(),
            parent_agent_id: None,
            root_session_id: parent_session.clone(),
            depth: 1,
            task: "tests".into(),
            prompt: "run tests".into(),
            manifest: manifest.clone(),
            state: DelegationState::Spawned,
            report: None,
        })
        .await
        .expect("create delegation");
    store
        .mark_delegation_running(agent.clone())
        .await
        .expect("mark child running");
    let mut parent_events = vec![
        envelope(
            generation,
            &device_id,
            &parent_session,
            &parent_run,
            "tool-start",
            EventPayload::Item(ItemEvent::Started {
                item_id: tool_item.clone(),
                item: TurnItem::ToolCall {
                    call_id: "spawn-call".into(),
                    name: "spawn_subagent".into(),
                    args: serde_json::json!({}),
                    status: ToolStatus::InProgress,
                },
            }),
        ),
        envelope(
            generation,
            &device_id,
            &parent_session,
            &parent_run,
            "tool-args",
            EventPayload::Item(ItemEvent::Delta {
                item_id: tool_item.clone(),
                delta: ItemDelta::ToolArgs {
                    fragment: r#"{"task":"tests","prompt":"run tests"}"#.into(),
                },
            }),
        ),
        envelope(
            generation,
            &device_id,
            &parent_session,
            &parent_run,
            "spawned",
            EventPayload::AgentSpawned(manifest),
        ),
        envelope(
            generation,
            &device_id,
            &parent_session,
            &parent_run,
            "waiting",
            EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::LocalChild,
            }),
        ),
    ];
    StoreHandle::append(&store, &mut parent_events)
        .await
        .expect("append parent wait");
    let mut child_done = [envelope(
        generation,
        &device_id,
        &child_session,
        &child_run,
        "child-done",
        EventPayload::RunState(RunState::Done),
    )];
    child_done[0].agent_id = Some(agent.clone());
    StoreHandle::append(&store, &mut child_done)
        .await
        .expect("append child done");
    store.close().await.expect("close generation one");

    // `close()` can return before the profile lock fully releases under
    // parallel test load (StoreLocked is self-declared RETRYABLE) —
    // bounded retry instead of a race flake (gate27 hygiene precedent).
    let recovered_store = {
        let mut attempt = 0;
        loop {
            match SqliteStoreHandle::open(root.path()).await {
                Ok(store) => break store,
                Err(error) if error.retryable && attempt < 40 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("open generation two: {error:?}"),
            }
        }
    };
    let work = recover_interrupted_turns(&recovered_store, &device_id)
        .await
        .expect("recover turns");
    let checkpoint = work
        .into_iter()
        .find_map(|work| match work {
            RecoveredWork::ChildWait(recovered) => Some(recovered.checkpoint),
            _ => None,
        })
        .expect("parent child wait recovered");
    assert_eq!(checkpoint.tools.len(), 1);
    assert_eq!(checkpoint.tools[0].call_id, "spawn-call");
    assert_eq!(checkpoint.tools[0].ticket.manifest.agent, agent);
    assert!(!checkpoint.tools[0].tool_result_emitted);
    let parent_tail = recovered_store
        .read(&parent_session, 0, 128)
        .await
        .expect("read recovered parent");
    assert!(matches!(
        parent_tail.last().map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).expect("payload")
        }),
        Some(EventPayload::RunState(RunState::Waiting {
            reason: WaitReason::LocalChild
        }))
    ));
    recovered_store.close().await.expect("close recovery store");
}

#[tokio::test]
async fn work_finishing_inside_the_barrier_completes_normally() {
    let (_sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let completed = bounded_finalization(
        async {
            tokio::task::yield_now().await;
            "done"
        },
        deadline,
        &mut shutdown,
    )
    .await;
    assert_eq!(completed, Some("done"));
    assert!(!barrier_breached(deadline, &shutdown));
}

/// The regression this pins: `forced` raised for a reason OTHER than this
/// step's own barrier — an undelivered drain notice, counted before
/// finalization runs — must not swallow an unrelated store failure. Before the
/// W3b1.5 refactor the flag was checked directly, so a notice-only force hid a
/// real flush error; the outcome said `Forced` and the error vanished.
///
/// MUTATION CHECK: in `barrier_step`, change the guard back to
/// `StepFailure::SuppressedWhenForced if *forced`. Expected failure: this test
/// gets `None` where it demands the store error.
#[tokio::test]
async fn a_force_raised_elsewhere_does_not_swallow_a_store_error() {
    let (_sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    // The drain already decided the outcome is forced (a connection never got
    // its ServerDraining), and the barrier itself is entirely intact.
    let mut forced = true;

    let reported = barrier_step(
        async {
            Err::<(), haider_protocol::error::HaiderError>(
                haider_protocol::error::HaiderError::new(
                    haider_protocol::error::ErrorCode::Internal,
                    "flush failed on its own",
                    false,
                ),
            )
        },
        None,
        StepFailure::SuppressedWhenForced,
        deadline,
        &mut shutdown,
        &mut forced,
    )
    .await;

    assert!(
        matches!(reported, Some(DaemonError::Store(_))),
        "a store failure unrelated to the barrier must still be reported, got {reported:?}"
    );
    assert!(forced, "the caller's reason for forcing still stands");
}

/// The other half of the same rule: when the step ITSELF ran into the barrier,
/// its failure is the expected consequence of the forced path (R17) and is not
/// the daemon's report.
#[tokio::test]
async fn a_step_that_ran_into_the_barrier_keeps_its_failure_to_itself() {
    let (sender, mut shutdown) = shutdown_channel();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    sender.send_replace(ShutdownRequest::Forced {
        reason: "second signal".into(),
    });
    let mut forced = false;

    let reported = barrier_step(
        async {
            Err::<(), haider_protocol::error::HaiderError>(
                haider_protocol::error::HaiderError::new(
                    haider_protocol::error::ErrorCode::Internal,
                    "flush failed under a force",
                    false,
                ),
            )
        },
        None,
        StepFailure::SuppressedWhenForced,
        deadline,
        &mut shutdown,
        &mut forced,
    )
    .await;

    assert!(reported.is_none(), "a forced path is lossy by contract");
    assert!(forced, "and the step's own breach raises the flag");
}

/// An always-reported step (endpoint cleanup) reports through a breach: a
/// rendezvous node the daemon could not remove outlives the process.
#[tokio::test]
async fn an_always_reported_step_survives_a_breached_barrier() {
    let (_sender, mut shutdown) = shutdown_channel();
    let expired = tokio::time::Instant::now() - Duration::from_secs(1);
    let mut forced = false;

    let reported = barrier_step(
        std::future::ready(Err::<(), DaemonError>(DaemonError::Endpoint {
            message: "socket still there".into(),
        })),
        None,
        StepFailure::AlwaysReported,
        expired,
        &mut shutdown,
        &mut forced,
    )
    .await;

    assert!(matches!(reported, Some(DaemonError::Endpoint { .. })));
    assert!(forced, "an expired deadline still forces the outcome");
}

/// MUTATION CHECK: reconstruct `AcceptedTurn` with `branch_id: None`, compare
/// aggregate `SessionState` scope before payload type, or emit recovery
/// terminals through an unscoped envelope. Expected RUNTIME failure: recovery
/// rejects the real acceptance-shaped aggregate or an A/B run moves to main.
#[tokio::test]
async fn restart_recovery_keeps_interleaved_runs_on_their_accepted_branches() {
    use crate::session_hub::{SessionHub, SessionHubConfig};
    use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
    use crate::worker::{
        BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
    };
    use haider_core::{
        BranchCreateCommand, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
        TurnAcceptCommand, TurnAcceptOutcome,
    };
    use haider_protocol::DeliveryMode;
    use haider_protocol::EventPayload;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{BranchId, DeviceId, EventId, RunId, SessionId};
    use haider_protocol::provider::FinishReason;
    use haider_protocol::session::SessionMetadataV1;
    use haider_protocol::state::{RunState, SessionState};
    use haider_provider::{FakeProvider, FakeStep, Provider};
    use std::sync::Arc;

    struct RecoveryProviderFactory {
        provider: Arc<dyn Provider>,
    }

    #[async_trait::async_trait]
    impl ProviderFactory for RecoveryProviderFactory {
        async fn resolve_for_turn(
            &self,
            metadata: &SessionMetadataV1,
        ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
            Ok(ResolvedTurnProvider {
                provider: Arc::clone(&self.provider),
                provider_name: metadata.provider.clone(),
                model: metadata.model.clone(),
                context_window: None,
                account_alias: None,
                active_no_auth: false,
                initial_rotation: None,
                rotation_budget_consumed: false,
                attempt_resolver: None,
                compaction_promotion: None,
            })
        }
    }

    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("branch-recovery-session");
    let device_id = DeviceId::new("branch-recovery-device");
    let branch_a = BranchId::new("branch-a");
    let branch_b = BranchId::new("branch-b");
    let run_a = RunId::new("queued-a");
    let run_b = RunId::new("queued-b");
    let cancelling_b = RunId::new("cancelling-b");
    let first = SqliteStoreHandle::open(root.path())
        .await
        .expect("open first");
    first
        .create_session(SessionCreateCommand {
            command_id: "create-branch-recovery".into(),
            request_digest: "create-branch-recovery-digest".into(),
            request_json: r#"{"session":"branch-recovery"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new("created-branch-recovery"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
    let generation = first.worker_generation();
    let source_run = RunId::new("branch-recovery-source");
    let TurnAcceptOutcome::Committed { .. } = first
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-branch-recovery-source".into(),
            request_digest: "accept-branch-recovery-source-digest".into(),
            request_json: r#"{"turn":"branch-recovery-source"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: source_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "stable recovery fork".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("branch-recovery-source-queued"),
            user_event_id: EventId::new("branch-recovery-source-user"),
            active_event_id: EventId::new("branch-recovery-source-active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept source")
    else {
        panic!("fresh source acceptance");
    };
    let mut source_done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("branch-recovery-source-done"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(source_run.clone()),
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
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("source done payload"),
    }];
    StoreHandle::append(&first, &mut source_done)
        .await
        .expect("finish source");
    let source_events = StoreHandle::read(&first, &session_id, 0, 64)
        .await
        .expect("read source");
    let (fork_node, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&source_run)).then_some((node.node, event.seq))
        })
        .expect("source node");
    for (command_id, branch_id) in [
        ("create-recovery-a", branch_a.clone()),
        ("create-recovery-b", branch_b.clone()),
    ] {
        let request_json = serde_json::json!({"branch": branch_id}).to_string();
        first
            .create_branch(BranchCreateCommand {
                command_id: command_id.into(),
                request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json,
                session_id: session_id.clone(),
                worker_generation: generation,
                branch_id,
                source_branch_id: None,
                fork_node_id: fork_node.clone(),
                fork_seq,
                name: None,
                event_id: EventId::new(format!("event-{command_id}")),
                device_id: device_id.clone(),
            })
            .await
            .expect("create recovery branch");
    }
    let event = |id: &str, run_id: &RunId, branch_id: &BranchId, payload: EventPayload| {
        let prompt = if matches!(&payload, EventPayload::UserMessage { .. }) {
            PromptRender::Verbatim
        } else {
            PromptRender::Omit
        };
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: Some(branch_id.clone()),
            run_id: Some(run_id.clone()),
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
                prompt,
            },
            payload: serde_json::to_value(payload).expect("payload"),
        }
    };
    let user = |text: &str| EventPayload::UserMessage {
        text: text.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
    };
    let mut aggregate_active = event(
        "a-aggregate-active",
        &run_a,
        &branch_a,
        EventPayload::SessionState(SessionState::ActiveRun),
    );
    aggregate_active.branch_id = None;
    let mut accepted = vec![
        event(
            "a-queued",
            &run_a,
            &branch_a,
            EventPayload::RunState(RunState::Queued),
        ),
        event("a-user", &run_a, &branch_a, user("A")),
        aggregate_active,
        event(
            "b-queued",
            &run_b,
            &branch_b,
            EventPayload::RunState(RunState::Queued),
        ),
        event("b-user", &run_b, &branch_b, user("B")),
        event("cancel-user", &cancelling_b, &branch_b, user("cancel B")),
        event(
            "cancel-state",
            &cancelling_b,
            &branch_b,
            EventPayload::RunState(RunState::Cancelling),
        ),
    ];
    StoreHandle::append(&first, &mut accepted)
        .await
        .expect("append old-generation runs");
    first.close().await.expect("close first");

    let recovered = SqliteStoreHandle::open(root.path()).await.expect("reopen");
    let work = recover_interrupted_turns(&recovered, &DeviceId::new("recovery-worker"))
        .await
        .expect("recover");
    let queued = work
        .into_iter()
        .filter_map(|work| match work {
            RecoveredWork::Queued(accepted) => Some(accepted),
            RecoveredWork::Retry(_)
            | RecoveredWork::Checkpoint(_)
            | RecoveredWork::PartialStream(_)
            | RecoveredWork::ChildWait(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queued
            .iter()
            .filter_map(|accepted| accepted.branch_id.clone())
            .collect::<Vec<_>>(),
        vec![branch_a.clone(), branch_b.clone()]
    );

    let events = StoreHandle::read(&recovered, &session_id, 0, 256)
        .await
        .expect("read recovery journal");
    assert!(events.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&cancelling_b)
            && envelope.branch_id.as_ref() == Some(&branch_b)
            && matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::RunState(RunState::Cancelled))
            )
    }));
    assert!(events.iter().all(|envelope| {
        !matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::SessionState(
                SessionState::ActiveRun | SessionState::Idle { .. }
            ))
        ) || envelope.branch_id.is_none()
    }));

    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "recovered A".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "recovered B".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(recovered.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(RecoveryProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install manager");
    for accepted in queued {
        handle
            .recover_queued(accepted)
            .await
            .expect("start recovered turn");
    }
    let recovery_completion = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = StoreHandle::read(&recovered, &session_id, 0, 512)
                .await
                .expect("read recovered workers");
            let done = [(&run_a, &branch_a), (&run_b, &branch_b)].into_iter().all(
                |(run_id, branch_id)| {
                    events.iter().any(|event| {
                        event.run_id.as_ref() == Some(run_id)
                            && event.branch_id.as_ref() == Some(branch_id)
                            && matches!(
                                serde_json::from_value::<EventPayload>(event.payload.clone()),
                                Ok(EventPayload::RunState(RunState::Done))
                            )
                    })
                },
            );
            if done {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if recovery_completion.is_err() {
        let tail = StoreHandle::read(&recovered, &session_id, 0, 512)
            .await
            .expect("read failed recovery tail")
            .into_iter()
            .filter(|event| {
                event.run_id.as_ref() == Some(&run_a) || event.run_id.as_ref() == Some(&run_b)
            })
            .map(|event| {
                (
                    event.seq,
                    event.run_id,
                    event.branch_id,
                    serde_json::from_value::<EventPayload>(event.payload),
                )
            })
            .collect::<Vec<_>>();
        panic!("recovered workers did not finish: {tail:?}");
    }
    let started_events = StoreHandle::read(&recovered, &session_id, 0, 512)
        .await
        .expect("read completed recovery");
    for (run_id, branch_id) in [(&run_a, &branch_a), (&run_b, &branch_b)] {
        assert!(started_events.iter().any(|event| {
            event.run_id.as_ref() == Some(run_id)
                && event.branch_id.as_ref() == Some(branch_id)
                && matches!(
                    serde_json::from_value::<EventPayload>(event.payload.clone()),
                    Ok(EventPayload::Item(_) | EventPayload::NodeCommitted(_))
                )
        }));
    }
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    recovered.close().await.expect("close recovered");
}

/// MUTATION CHECK: drop the accepted branch from the worker dispatch loop's
/// failed-start terminalization locals. Expected RUNTIME failure: a
/// recovering branch run whose provider resolution fails terminalizes as
/// `Errored` without its branch stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_recovery_start_terminalizes_on_the_accepted_branch() {
    use crate::session_hub::{SessionHub, SessionHubConfig};
    use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};
    use crate::worker::{
        BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
    };
    use haider_core::{
        BranchCreateCommand, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
        TurnAcceptCommand, TurnAcceptOutcome,
    };
    use haider_protocol::DeliveryMode;
    use haider_protocol::EventPayload;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::error::{ErrorCode, HaiderError};
    use haider_protocol::ids::{BranchId, DeviceId, EventId, RunId, SessionId};
    use haider_protocol::session::SessionMetadataV1;
    use haider_protocol::state::{RunState, SessionState};

    struct FailingProviderFactory;

    #[async_trait::async_trait]
    impl ProviderFactory for FailingProviderFactory {
        async fn resolve_for_turn(
            &self,
            _metadata: &SessionMetadataV1,
        ) -> Result<ResolvedTurnProvider, HaiderError> {
            Err(HaiderError::new(
                ErrorCode::Internal,
                "failed-start fixture refuses every provider resolution",
                false,
            ))
        }
    }

    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("failed-start-branch-session");
    let device_id = DeviceId::new("failed-start-branch-device");
    let branch_id = BranchId::new("failed-start-branch");
    let run_id = RunId::new("failed-start-run");
    let first = SqliteStoreHandle::open(root.path())
        .await
        .expect("open first");
    first
        .create_session(SessionCreateCommand {
            command_id: "create-failed-start".into(),
            request_digest: "create-failed-start-digest".into(),
            request_json: r#"{"session":"failed-start"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::env::current_dir()
                .expect("cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new("created-failed-start"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
    let generation = first.worker_generation();
    let source_run = RunId::new("failed-start-source");
    let TurnAcceptOutcome::Committed { .. } = first
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-failed-start-source".into(),
            request_digest: "accept-failed-start-source-digest".into(),
            request_json: r#"{"turn":"failed-start-source"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: source_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "stable failed-start fork".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("failed-start-source-queued"),
            user_event_id: EventId::new("failed-start-source-user"),
            active_event_id: EventId::new("failed-start-source-active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept source")
    else {
        panic!("fresh source acceptance");
    };
    let stamp = |id: &str, run: &RunId, branch: Option<&BranchId>, payload: EventPayload| {
        let prompt = if matches!(&payload, EventPayload::UserMessage { .. }) {
            PromptRender::Verbatim
        } else {
            PromptRender::Omit
        };
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: branch.cloned(),
            run_id: Some(run.clone()),
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
                prompt,
            },
            payload: serde_json::to_value(payload).expect("payload"),
        }
    };
    let mut source_done = [stamp(
        "failed-start-source-done",
        &source_run,
        None,
        EventPayload::RunState(RunState::Done),
    )];
    StoreHandle::append(&first, &mut source_done)
        .await
        .expect("finish source");
    let source_events = StoreHandle::read(&first, &session_id, 0, 64)
        .await
        .expect("read source");
    let (fork_node, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&source_run)).then_some((node.node, event.seq))
        })
        .expect("source node");
    let request_json = serde_json::json!({"branch": branch_id}).to_string();
    first
        .create_branch(BranchCreateCommand {
            command_id: "create-failed-start-branch".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: generation,
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: fork_node,
            fork_seq,
            name: None,
            event_id: EventId::new("event-create-failed-start-branch"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create branch");
    let mut aggregate_active = stamp(
        "failed-start-aggregate-active",
        &run_id,
        None,
        EventPayload::SessionState(SessionState::ActiveRun),
    );
    aggregate_active.branch_id = None;
    let mut accepted = vec![
        stamp(
            "failed-start-queued",
            &run_id,
            Some(&branch_id),
            EventPayload::RunState(RunState::Queued),
        ),
        stamp(
            "failed-start-user",
            &run_id,
            Some(&branch_id),
            EventPayload::UserMessage {
                text: "branch run that cannot start".into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Queue,
            },
        ),
        aggregate_active,
    ];
    StoreHandle::append(&first, &mut accepted)
        .await
        .expect("append old-generation run");
    first.close().await.expect("close first");

    let recovered = SqliteStoreHandle::open(root.path()).await.expect("reopen");
    let work = recover_interrupted_turns(&recovered, &DeviceId::new("failed-start-worker"))
        .await
        .expect("recover");
    let queued = work
        .into_iter()
        .filter_map(|work| match work {
            RecoveredWork::Queued(accepted) => Some(accepted),
            RecoveredWork::Retry(_)
            | RecoveredWork::Checkpoint(_)
            | RecoveredWork::PartialStream(_)
            | RecoveredWork::ChildWait(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queued
            .iter()
            .map(|accepted| accepted.branch_id.clone())
            .collect::<Vec<_>>(),
        vec![Some(branch_id.clone())]
    );

    let hub = SessionHub::new(recovered.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(FailingProviderFactory),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install manager");
    for accepted in queued {
        handle
            .recover_queued(accepted)
            .await
            .expect("queue recovered turn");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let errored = loop {
        let events = StoreHandle::read(&recovered, &session_id, 0, 256)
            .await
            .expect("read journal");
        let errored = events.iter().find_map(|envelope| {
            (envelope.run_id.as_ref() == Some(&run_id)
                && matches!(
                    serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                    Ok(EventPayload::RunState(RunState::Errored))
                ))
            .then(|| envelope.branch_id.clone())
        });
        if let Some(branch) = errored {
            break branch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "failed start never terminalized the branch run"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(errored, Some(branch_id));
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    recovered.close().await.expect("close recovered");
}
