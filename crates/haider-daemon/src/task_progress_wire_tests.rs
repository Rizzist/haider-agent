#![allow(clippy::expect_used)]

use super::*;
use crate::session_hub::{FrameSendError, FrameSink, HubConnection, SessionHubConfig};
use haider_core::SqliteStoreHandle;
use haider_protocol::ids::DeviceId;
use haider_rpc::{AttachMode, Capability, RequestBody, RequestId, ResponseBody, WireFrame};
use std::collections::BTreeSet;

#[derive(Default)]
struct WireSink(StdMutex<Vec<WireFrame>>);
impl FrameSink for WireSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        // Inspect what survives actual wire serialization, not registry values.
        let bytes = serde_json::to_vec(&frame).expect("encode frame");
        self.0
            .lock()
            .expect("frames")
            .push(serde_json::from_slice(&bytes).expect("decode frame"));
        Ok(())
    }
}

async fn observe(
    connection: &HubConnection,
    sink: &WireSink,
    session: &SessionId,
    batch: bool,
) -> Vec<haider_rpc::ObserveTaskWire> {
    connection
        .request(
            RequestId::new("progress-read"),
            if batch {
                RequestBody::SessionObserveBatch {
                    session_ids: vec![session.clone()],
                    last_event_limit: 0,
                    metadata_only: false,
                }
            } else {
                RequestBody::SessionObserve {
                    session_id: session.clone(),
                    last_event_limit: 0,
                    metadata_only: false,
                }
            },
        )
        .await
        .expect("observe request");
    sink.0
        .lock()
        .expect("frames")
        .iter()
        .rev()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body: ResponseBody::SessionObserve { digest },
                ..
            } => digest.tasks.clone(),
            WireFrame::Response {
                body: ResponseBody::SessionObserveBatch { digests },
                ..
            } => digests[0].tasks.clone(),
            _ => None,
        })
        .expect("task snapshot on wire")
}

#[tokio::test]
async fn activity_task_progress_wire_coalesces_bursts_and_emits_terminal_immediately() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session = SessionId::new("progress-session");
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: "progress-create".into(),
        request_digest: "progress-create-digest".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        cwd: root.path().to_string_lossy().into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "progress-test".into(),
        event_id: EventId::new("progress-created"),
        device_id: DeviceId::new("progress-test"),
    })
    .await
    .expect("session");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "progress-turn".into(),
        request_digest: "progress-turn-digest".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: hub.worker_generation(),
        run_id: RunId::new("progress-run"),
        agent_id: None,
        branch_id: None,
        text: "progress fixture".into(),
        attachments: vec![],
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new("progress-queued"),
        user_event_id: EventId::new("progress-user"),
        active_event_id: EventId::new("progress-active"),
        device_id: DeviceId::new("progress-test"),
    })
    .await
    .expect("accepted task run");
    let sink = Arc::new(WireSink::default());
    let connect = || {
        hub.open_connection(
            BTreeSet::from([Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection")
    };
    let first = connect();
    first
        .request(
            RequestId::new("progress-attach"),
            RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach for terminal events");
    let task = TaskId::new("progress-task");
    let output = shared_task_output(TASK_OUTPUT_RETAIN_BYTES, TASK_TAIL_BYTES);
    let registry = hub.task_registry();
    registry.begin_adoption(&session);
    registry.insert(
        &session,
        TaskEntry {
            task: task.clone(),
            name: "progress".into(),
            pid: 0,
            started_at_ms: now_ms(),
            state: TaskLiveState::Running,
            run_id: RunId::new("progress-run"),
            branch_id: None,
            agent_id: None,
            output: Some(output.clone()),
            kill: None,
            terminal_fact: None,
        },
    );
    tokio::time::pause();
    // SQLite uses an OS thread. Keep the runtime runnable while requests await
    // it so paused time advances ONLY at the explicit steps below.
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let start = tokio::time::Instant::now();
    lock_task_output(&output).append(b"initial\n");
    let initial = observe(&first, &sink, &session, false).await;
    assert_eq!(initial[0].last_line.as_deref(), Some("initial"));
    let second = connect(); // A fresh connection must share the same deadline.
    let mut emissions = vec![initial[0].clone()];
    for index in 1..100 {
        tokio::time::advance(Duration::from_millis(10)).await;
        lock_task_output(&output).append(format!("burst {index}\n").as_bytes());
        let rows = observe(
            if index % 2 == 0 { &first } else { &second },
            &sink,
            &session,
            index % 3 == 0,
        )
        .await;
        assert_eq!(rows.len(), 1);
        if emissions.last() != rows.first() {
            emissions.push(rows[0].clone());
        }
    }
    assert_eq!(
        tokio::time::Instant::now() - start,
        Duration::from_millis(990)
    );
    assert_eq!(
        emissions.len(),
        1,
        "at most one distinct progress emission in the first second"
    );
    tokio::time::advance(Duration::from_millis(10)).await;
    let coalesced = observe(&second, &sink, &session, true).await;
    assert_eq!(coalesced[0].last_line.as_deref(), Some("burst 99"));
    assert_eq!(coalesced[0].bytes, lock_task_output(&output).total_bytes());
    emissions.push(coalesced[0].clone());
    for index in 100..125 {
        tokio::time::advance(Duration::from_millis(10)).await;
        lock_task_output(&output).append(format!("burst {index}\n").as_bytes());
        let rows = observe(&first, &sink, &session, false).await;
        if emissions.last() != rows.first() {
            emissions.push(rows[0].clone());
        }
    }
    assert_eq!(emissions.len(), 2, "second burst must also coalesce");
    lock_task_output(&output).append(b"final output\n");
    let terminal_at = tokio::time::Instant::now();
    TaskFacade::new(hub.clone())
        .complete_task(
            &session,
            &task,
            BackgroundExitStatus {
                exit_code: Some(0),
                signal: None,
                killed: false,
                fault: None,
                workspace_mutation: None,
            },
        )
        .await;
    assert!(
        observe(&second, &sink, &session, true).await.is_empty(),
        "no stale row during the throttle window"
    );
    // Flush live delivery through the real attachment, without advancing time.
    let delivery_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let terminal = loop {
        assert!(
            std::time::Instant::now() < delivery_deadline,
            "terminal event delivered on attachment"
        );
        let terminal = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match frame {
                WireFrame::Event { envelope, .. } => {
                    TaskEventPayload::from_payload_value(&envelope.payload)
                }
                _ => None,
            })
            .filter_map(|event| match event {
                TaskEventPayload::TaskCompleted(fact) if fact.task == task => Some(fact),
                _ => None,
            })
            .next_back();
        if let Some(terminal) = terminal {
            break terminal;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(
        tokio::time::Instant::now(),
        terminal_at,
        "terminal emission must not wait for the progress deadline"
    );
    assert_eq!(
        terminal.state,
        TaskTerminalState::Completed { exit_code: Some(0) }
    );
    assert!(terminal.tail.ends_with("final output\n"));
    assert_eq!(
        terminal.output_bytes,
        lock_task_output(&output).total_bytes()
    );
    clock_guard.abort();
    tokio::time::resume();
    first.close().await.expect("close first");
    second.close().await.expect("close second");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}
