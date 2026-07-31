//! Unit tests for the drain barrier's arbitration helpers.
//!
//! These are crate-internal on purpose: `bounded_finalization` and
//! `barrier_breached` decide whether a shutdown may call itself graceful, and
//! the interesting cases (a step that completes exactly as the deadline
//! passes, a second signal that arrives during a step) are driven far more
//! precisely with synthetic futures than with a real daemon.

#![allow(clippy::expect_used)]

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
    };
    store
        .create_delegation(DelegationRecord {
            agent_id: agent.clone(),
            child_session_id: child_session.clone(),
            child_run_id: child_run.clone(),
            parent_session_id: parent_session.clone(),
            parent_run_id: parent_run.clone(),
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

    let recovered_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("open generation two");
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
        StepFailure::AlwaysReported,
        expired,
        &mut shutdown,
        &mut forced,
    )
    .await;

    assert!(matches!(reported, Some(DaemonError::Endpoint { .. })));
    assert!(forced, "an expired deadline still forces the outcome");
}
