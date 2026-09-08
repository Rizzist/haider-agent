#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The standalone feature uses the real runtime, UDS decoder, hub and actors.
use crate::*;
use haider_protocol::{
    EventPayload, envelope::RawEnvelope, provider::FinishReason, session::SessionMetadataV1,
};
use haider_provider::{FakeInputKind, FakeInputOption, FakeProvider, FakeStep, Provider};
use haider_rpc::{ResponseBody, WireFrame};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Factory(Arc<FakeProvider>);
#[async_trait::async_trait]
impl ProviderFactory for Factory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.0) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            active_no_auth: true,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct Client {
    stream: haider_platform::IpcStream,
    decoder: haider_rpc::uds_codec::Decoder,
    frames: VecDeque<WireFrame>,
    events: VecDeque<RawEnvelope>,
    request: usize,
}
impl Client {
    async fn new(path: &Path) -> Self {
        Self {
            stream: haider_platform::connect(path).await.expect("UDS connect"),
            decoder: haider_rpc::uds_codec::Decoder::new(8_388_608),
            frames: VecDeque::new(),
            events: VecDeque::new(),
            request: 0,
        }
    }
    async fn send(&mut self, frame: Value) {
        let bytes = serde_json::to_vec(&frame).expect("fixture JSON");
        self.stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .expect("prefix");
        self.stream.write_all(&bytes).await.expect("body");
    }
    async fn next(&mut self) -> WireFrame {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(frame) = self.frames.pop_front() {
                    return frame;
                }
                let mut bytes = [0; 8192];
                let count = self.stream.read(&mut bytes).await.expect("frame read");
                assert_ne!(count, 0, "unexpected EOF");
                let batch = self.decoder.push(&bytes[..count]);
                assert!(
                    batch.error.is_none(),
                    "invalid server frame: {:?}",
                    batch.error
                );
                self.frames.extend(batch.frames);
            }
        })
        .await
        .expect("RPC deadline")
    }
    async fn hello(&mut self, min: u32) -> WireFrame {
        self.send(json!({"v":1,"kind":"hello","protocol_min":min,"protocol_max":min,
            "client_name":"android-contract-test","client_version":"971","client_instance_id":"fixture",
            "client_kind":"gui","capabilities_requested":["view","control"],"max_receive_frame":8_388_608})).await;
        self.next().await
    }
    async fn request(&mut self, body: Value) -> ResponseBody {
        self.request += 1;
        let id = format!("android-{}", self.request);
        self.send(json!({"v":1,"kind":"request","request_id":id,"body":body}))
            .await;
        loop {
            match self.next().await {
                WireFrame::Response { request_id, body } if request_id.as_str() == id => {
                    return body;
                }
                WireFrame::Event { envelope, .. } => self.events.push_back(envelope),
                _ => (),
            }
        }
    }
    async fn event(&mut self) -> RawEnvelope {
        loop {
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            if let WireFrame::Event { envelope, .. } = self.next().await {
                return envelope;
            }
        }
    }
}

async fn ready(config: DaemonConfig, dependencies: DaemonDependencies) -> DaemonTask {
    let task = spawn_with_dependencies(config, dependencies);
    let mut readiness = task.readiness();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !readiness.snapshot().ready {
            assert!(
                !matches!(
                    readiness.current(),
                    DaemonState::Failed { .. } | DaemonState::Stopped
                ),
                "{:?}",
                task.diagnostics().snapshot()
            );
            readiness.changed().await.expect("ready publisher");
        }
    })
    .await
    .expect("Ready deadline");
    task
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_runtime_full_rpc_recovery_metadata_inventory_and_force() {
    let root = tempfile::Builder::new()
        .prefix("r971")
        .tempdir_in("/tmp")
        .expect("root");
    let root = root.path().canonicalize().expect("canonical root");
    let store = root.join("store");
    std::fs::create_dir(&store).expect("store");
    let mut config = DaemonConfig::new("android-contract", &store, root.join("r"));
    config.android_workspace_dir = Some(crate::android_workspace::test_root().into());
    config.lockdown_root_override = Some(store.join("lockdown"));
    config.store_synchronous = Some(haider_protocol::runtime::StoreSynchronous::Normal);
    config.discovery_disabled = true;
    config.frame_limit = 8_388_608;
    config.outbound_queued_bytes = 8_388_612;
    config.max_connections = 4;
    config.outbound_queue_capacity = 8;
    let fake = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "excluded".into(),
                name: "exec".into(),
                args: json!({"command":"touch forbidden"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitRequestInput {
                call_id: "input".into(),
                kind: FakeInputKind::Choice,
                title: "Synthetic choice".into(),
                body: vec![],
                options: vec![FakeInputOption {
                    key: "yes".into(),
                    label: "Continue".into(),
                    detail: None,
                }],
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "standalone fake completion".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(Arc::new(std::sync::Mutex::new(
            haider_platform::RouteStatus::Available,
        ))),
    );
    let vault = haider_accounts::EncryptedFileVault::new(
        store.join("vault"),
        zeroize::Zeroizing::new([71; 32]),
    )
    .expect("encrypted vault");
    let mut dependencies = DaemonDependencies::default();
    dependencies.provider_factory =
        ProviderFactoryConfig::injected(Arc::new(Factory(Arc::clone(&fake))));
    dependencies.accounts.vault = VaultProvision::Available(Arc::new(vault));
    let task = ready(config.clone(), dependencies.clone()).await;
    let diagnostics = task.diagnostics();
    let metadata = diagnostics
        .snapshot()
        .bootstrap
        .expect("real bootstrap metadata");
    let count = diagnostics.snapshot().connection_admissions;
    for _ in 0..100 {
        assert_eq!(diagnostics.snapshot().bootstrap.as_ref(), Some(&metadata));
    }
    assert_eq!(
        diagnostics.snapshot().connection_admissions,
        count,
        "observing must not self-connect"
    );
    assert_eq!(metadata.endpoint_path, config.runtime_dir.join("h.sock"));
    let loser = spawn_with_dependencies(config.clone(), dependencies.clone());
    assert!(matches!(
        loser.join().await,
        Err(DaemonError::AlreadyRunning { .. })
    ));
    let mut incompatible = Client::new(&metadata.endpoint_path).await;
    assert!(matches!(
        incompatible.hello(99).await,
        WireFrame::ProtocolError(_)
    ));
    drop(incompatible);
    let mut client = Client::new(&metadata.endpoint_path).await;
    let WireFrame::Welcome(welcome) = client.hello(1).await else {
        panic!("Welcome required");
    };
    assert_eq!(welcome.daemon_generation, metadata.daemon_generation);
    let created = client.request(json!({"method":"session.create","command_id":"create","cwd":config.android_workspace_dir,
        "provider":"fake","model":"fake-model","max_tokens":4096})).await;
    let ResponseBody::SessionCreate {
        session_id,
        worker_generation,
        ..
    } = created
    else {
        panic!("create: {created:?}");
    };
    assert!(matches!(
        client
            .request(json!({"method":"session.list","limit":20}))
            .await,
        ResponseBody::SessionList { .. }
    ));
    assert!(matches!(
        client.request(json!({"method":"session.list_watch"})).await,
        ResponseBody::SessionListWatch { .. }
    ));
    let inventory = client
        .request(json!({"method":"tools.inventory","session_id":session_id}))
        .await;
    let ResponseBody::ToolsInventory { inventory, .. } = inventory else {
        panic!("inventory");
    };
    assert!(!inventory.tools.iter().any(|entry| matches!(
        entry.manifest.name.as_str(),
        "process_exec" | "exec" | "computer" | "peer_list" | "ssh_shell"
    )));
    let outside = client
        .request(
            json!({"method":"session.workspace.set","command_id":"outside","session_id":session_id,
        "worker_generation":worker_generation,"path":store}),
        )
        .await;
    assert!(matches!(outside, ResponseBody::Error { .. }));
    for body in [
        json!({"method":"shell.list"}),
        json!({"method":"peer.list"}),
        json!({"method":"account.source_scan"}),
        json!({"method":"transcription.secret_get"}),
    ] {
        let method = body["method"].clone();
        let reply = client.request(body).await;
        assert!(
            matches!(&reply, ResponseBody::Error { code, .. } if code == "capability_denied"),
            "{method}: {reply:?}"
        );
    }
    assert!(matches!(client.request(json!({"method":"session.attach","session_id":session_id,"after_seq":0,"mode":"control"})).await, ResponseBody::SessionAttach { .. }));
    assert!(matches!(
        client
            .request(
                json!({"method":"turn.submit","command_id":"submit","session_id":session_id,
        "worker_generation":worker_generation,"text":"synthetic turn","mode":"queue"})
            )
            .await,
        ResponseBody::TurnSubmit { .. }
    ));
    let (menu, seq) = loop {
        let event = client.event().await;
        if let Ok(EventPayload::MenuOpened(menu)) = event.payload.decode_event() {
            break (menu, event.seq);
        }
    };
    client.send(json!({"v":1,"kind":"menu_answer","request_id":"answer","command_id":"answer",
        "session_id":session_id,"menu_id":menu.id,"request_seq":seq,"worker_generation":worker_generation,
        "option_key":menu.options[0].key,"option_index":0,"input":null})).await;
    loop {
        let event = client.event().await;
        if matches!(
            event.payload.decode_event(),
            Ok(EventPayload::RunState(
                haider_protocol::state::RunState::Done
            ))
        ) {
            break;
        }
    }
    assert!(
        fake.requests()
            .iter()
            .flat_map(|request| &request.messages)
            .any(|message| format!("{message:?}").contains("unsupported tool `exec`"))
    );
    drop(client);
    task.crash().await;
    let recovered = ready(config.clone(), dependencies.clone()).await;
    assert!(
        recovered
            .diagnostics()
            .snapshot()
            .bootstrap
            .expect("recovery metadata")
            .daemon_generation
            > metadata.daemon_generation
    );
    recovered.shutdown_handle().request_graceful();
    assert_eq!(
        recovered.join().await.expect("graceful"),
        ShutdownOutcome::Graceful
    );
    assert!(!config.runtime_dir.join("h.sock").exists());
    assert!(!config.runtime_dir.join("mobile.sock").exists());
    let database =
        rusqlite::Connection::open(store.join("store.sqlite")).expect("integrity connection");
    assert_eq!(
        database
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("integrity"),
        "ok"
    );
    drop(database);
    let forced = ready(config.clone(), dependencies).await;
    forced.shutdown_handle().request_graceful();
    forced.shutdown_handle().request("android-forced");
    assert_eq!(
        forced.join().await.expect("forced"),
        ShutdownOutcome::Forced
    );
}
