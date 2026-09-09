#![allow(clippy::expect_used)]

use super::*;
use haider_core::SqliteStoreHandle;
use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::ids::{DeviceId, EventId, LeaseId};

fn spawn_envelope(coordinates: Option<serde_json::Value>) -> RawEnvelope {
    let manifest = AgentManifest {
        agent: AgentId::new("activity-child"),
        role: AgentRole::Subagent,
        task: "review code".into(),
        callsign: None,
        model_profile: "fake".into(),
        grant: Grant {
            tools: vec![],
            effect_ceiling: vec![],
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new("activity-lease"),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates,
        cli_scope: None,
    };
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("activity-spawn"),
        seq: 1,
        session_id: SessionId::new("activity-session"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("activity-device"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::AgentSpawned(manifest))
            .expect("spawn")
            .into(),
    }
}

#[test]
fn activity_subagent_type_folds_from_durable_display_coordinates_without_guessing() {
    let frozen = serde_json::json!({"id":"reviewer", "name":"Code reviewer", "color":"#abcdef", "glyph":"✦"});
    for coordinates in [
        None,
        Some(serde_json::json!({"agent_type": frozen.clone()})),
        Some(serde_json::json!({"agent_type":"@reviewer"})),
    ] {
        let expected = coordinates
            .as_ref()
            .and_then(|value| value["agent_type"].as_object())
            .map(|_| frozen.clone());
        let envelope = spawn_envelope(coordinates);
        let mut projection = ObserveProjection::new(4);
        projection.apply(envelope.clone());
        let digest = projection.finish(envelope.session_id, 1, 1, None);
        assert_eq!(
            serde_json::to_value(&digest.subagents[0])
                .expect("child")
                .get("agent_type"),
            expected.as_ref()
        );
        assert!(
            digest.tasks.is_none(),
            "journal folds contain no live output"
        );
    }
}

struct InventorySink(tokio::sync::mpsc::UnboundedSender<haider_rpc::ShellInventoryWire>);
impl FrameSink for InventorySink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if let WireFrame::ShellInventoryChanged { inventory } = frame {
            self.0.send(inventory).map_err(|_| FrameSendError)?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn activity_shell_signal_coalesces_bursts_to_one_per_second_and_stops_on_shutdown() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    let connection = hub
        .open_connection(
            BTreeSet::from([haider_rpc::Capability::View]),
            Arc::new(InventorySink(send)),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    tokio::time::pause();
    let local = hub
        .shell_registry()
        .open(haider_rpc::ShellKindWire::Local, "one", ".")
        .expect("open");
    assert_eq!(
        events.recv().await.expect("initial change"),
        haider_rpc::ShellInventoryWire {
            count: 1,
            revision: 1
        }
    );
    let second = hub
        .shell_registry()
        .open(haider_rpc::ShellKindWire::Local, "two", ".")
        .expect("open");
    local.exited(Some(0)).expect("exit");
    local.add_output(10_000).expect("output");
    // Poll the consumer while the producer runs. advance + try_recv can
    // miss a ready signal before the producer gets scheduled.
    assert!(
        tokio::time::timeout(Duration::from_millis(999), events.recv())
            .await
            .is_err(),
        "no early signal"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        events.recv().await.expect("coalesced change"),
        haider_rpc::ShellInventoryWire {
            count: 1,
            revision: 3
        }
    );
    second.add_output(20_000).expect("output alone");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        events.try_recv().is_err(),
        "output cannot trigger count signals"
    );
    tokio::time::resume();
    connection.close().await.expect("close connection");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close store");
}

#[derive(Default)]
struct RefusingInventorySink(tokio::sync::Notify);
impl FrameSink for RefusingInventorySink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
        Ok(())
    }
    fn try_send_droppable(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        if matches!(frame, WireFrame::ShellInventoryChanged { .. }) {
            Err(FrameSendError)
        } else {
            Ok(())
        }
    }
    fn close_after_required_delivery_failure(&self) {
        self.0.notify_one();
    }
}

#[derive(Default)]
struct ObserveSink(std::sync::Mutex<Vec<WireFrame>>);
impl FrameSink for ObserveSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("observe frames").push(frame);
        Ok(())
    }
}

#[tokio::test]
async fn activity_refused_signal_closes_transport_and_reconnect_read_repairs_inventory() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session = SessionId::new("activity-reconnect");
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: "activity-create".into(),
        request_digest: "activity-create-digest".into(),
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
        system_prompt_version: "activity-test".into(),
        event_id: EventId::new("activity-created"),
        device_id: DeviceId::new("activity-test"),
    })
    .await
    .expect("session");
    let refusing = Arc::new(RefusingInventorySink::default());
    let first = hub
        .open_connection(
            BTreeSet::from([haider_rpc::Capability::View]),
            refusing.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection");
    let shell = hub
        .shell_registry()
        .open(haider_rpc::ShellKindWire::Local, "kept", ".")
        .expect("shell");
    tokio::time::timeout(Duration::from_secs(5), refusing.0.notified())
        .await
        .expect("required signal closes stalled transport");
    first.close().await.expect("disconnect");
    let sink = Arc::new(ObserveSink::default());
    let second = hub
        .open_connection(
            BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("reconnect");
    second
        .request(
            RequestId::new("activity-baseline"),
            RequestBody::SessionObserve {
                session_id: session,
                last_event_limit: 0,
                metadata_only: false,
            },
        )
        .await
        .expect("fresh baseline");
    let inventory = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body: ResponseBody::SessionObserve { digest },
                ..
            } => digest.shells,
            _ => None,
        })
        .expect("inventory baseline");
    assert_eq!(
        inventory,
        haider_rpc::ShellInventoryWire {
            count: 1,
            revision: 1
        }
    );
    assert_eq!(inventory, hub.shell_registry().inventory());
    shell.exited(Some(0)).expect("exit");
    second.close().await.expect("disconnect");
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("store close");
}
