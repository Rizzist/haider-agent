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
        serde_json::from_value::<haider_rpc::RequestBody>(body.clone())
            .expect("fixture must use the real request schema");
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
                WireFrame::ProtocolError(error) => panic!("request protocol error: {error:?}"),
                _ => (),
            }
        }
    }
    async fn event(&mut self) -> RawEnvelope {
        loop {
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            match self.next().await {
                WireFrame::Event { envelope, .. } => return envelope,
                WireFrame::ProtocolError(error) => panic!("event protocol error: {error:?}"),
                WireFrame::Response {
                    body: ResponseBody::Error { code, message, .. },
                    ..
                } => {
                    panic!("event command failed: {code}: {message}");
                }
                _ => (),
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
    let dependencies = DaemonDependencies {
        provider_factory: ProviderFactoryConfig::injected(Arc::new(Factory(Arc::clone(&fake)))),
        accounts: AccountsDependencies {
            vault: VaultProvision::Available(Arc::new(vault)),
            ..Default::default()
        },
        ..Default::default()
    };
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
        json!({"method":"account.source_list"}),
        json!({"method":"account.source_add","command_id":"source-denied","kind":"claude","root":store}),
        json!({"method":"account.import_device","command_id":"import-denied","candidate":"synthetic"}),
        json!({"method":"ssh.list"}),
        json!({"method":"ssh.test","name":"synthetic"}),
        json!({"method":"peer.send","to":"synthetic","message":"must not send"}),
        json!({"method":"hooks.trust","command_id":"trust-denied","digest":"synthetic"}),
        json!({"method":"loom.install.retry","job_id":"synthetic"}),
        json!({"method":"transcription.secret_get"}),
    ] {
        assert!(
            matches!(client.request(body).await, ResponseBody::Error { code, .. } if code == "capability_denied")
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
    let providers = client.request(json!({"method":"provider.list"})).await;
    let ResponseBody::ProviderList { revision, .. } = providers else {
        panic!("provider snapshot: {providers:?}");
    };
    let trusted = client
        .request(
            json!({"method":"provider.set_trust", "command_id":"recovery-full-trust",
        "name":"fake", "trust":"full", "expected_revision":revision}),
        )
        .await;
    assert!(
        matches!(trusted, ResponseBody::ProviderSetTrust { ref provider, .. }
        if provider.trust == haider_rpc::ProviderTrustWire::Full),
        "{trusted:?}"
    );
    drop(client);
    task.crash().await;
    let workspace =
        tempfile::tempdir_in(crate::android_workspace::test_root()).expect("checkpoint cwd");
    let outside = tempfile::tempdir().expect("out-of-scope recovered cwd");
    let forbidden =
        seed_recovered_checkpoint(&store, outside.path(), "outside", "deleted.txt").await;
    let reserved = seed_recovered_checkpoint(
        &store,
        workspace.path(),
        "reserved",
        ".haider-lockdown/deleted.txt",
    )
    .await;
    let permitted =
        seed_recovered_checkpoint(&store, workspace.path(), "permitted", "deleted.txt").await;
    let recovered = ready(config.clone(), dependencies.clone()).await;
    exercise_recovered_checkpoint_rpc(
        &recovered,
        &[
            (forbidden, outside.path().join("deleted.txt"), false),
            (
                reserved,
                workspace.path().join(".haider-lockdown/deleted.txt"),
                false,
            ),
            (permitted, workspace.path().join("deleted.txt"), true),
        ],
    )
    .await;
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

async fn seed_recovered_checkpoint(
    store_root: &Path,
    cwd: &Path,
    label: &str,
    target: &str,
) -> haider_protocol::ids::SessionId {
    use haider_core::{SqliteStoreHandle, StoreHandle};
    use haider_protocol::{
        checkpoint::{CheckpointKind, CheckpointOrigin},
        effect::{
            AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
            WorkspaceMutation,
        },
        envelope::{EventEnvelope, RawPayload, RenderTargets, SCHEMA_VERSION},
        ids::*,
    };
    let store = SqliteStoreHandle::open(store_root)
        .await
        .expect("seed recovered store");
    let session = SessionId::new(format!("recovered-{label}"));
    store
        .create_session(haider_store::SessionCreateCommand {
            command_id: format!("seed-{label}"),
            request_digest: format!("seed-{label}"),
            request_json: "{}".into(),
            session_id: session.clone(),
            cwd: cwd.display().to_string(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: Some(haider_protocol::session::SessionPermissionOverridesV1 {
                auto_allow: true,
                ..Default::default()
            }),
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("seed-created-{label}")),
            device_id: DeviceId::new("synthetic-device"),
        })
        .await
        .expect("recovered full-trust session");
    let run = RunId::new(format!("seed-run-{label}"));
    let mut cas = store.clone();
    let checkpoint = haider_tools::freeze_checkpoint(
        &mut cas,
        haider_tools::FreezeCheckpointInput {
            session_id: session.clone(),
            branch_id: None,
            run_id: run.clone(),
            effect_id: EffectId::new(format!("seed-effect-{label}")),
            call_id: label.into(),
            origin: CheckpointOrigin::Tool,
            source_checkpoint_id: None,
        },
        haider_tools::CheckpointCapture {
            kind: CheckpointKind::Delete,
            paths: vec![haider_tools::CheckpointCapturePath {
                path: target.into(),
                pre_bytes: Some(b"synthetic recovered deletion".to_vec()),
                pre_digest: Some(format!(
                    "blake3:{}",
                    blake3::hash(b"synthetic recovered deletion").to_hex()
                )),
                post_digest: None,
                truncated_reason: None,
            }],
            post_digest: format!("blake3:{}", blake3::hash(label.as_bytes()).to_hex()),
        },
    )
    .await
    .expect("checkpoint with durable preimage");
    // Recovery starts from a valid durable effect history. The store stamps
    // the checkpoint's revision from its preceding mutation outcome.
    let effect = checkpoint.effect_id.clone();
    let payloads = [
        EventPayload::Effect(EffectPhase::Intent(EffectIntent {
            effect: effect.clone(),
            class: EffectClass::FsWrite,
            summary: "synthetic recovered deletion".into(),
            args_digest: checkpoint.post_digest.clone(),
            workspace_revision: None,
        })),
        EventPayload::Effect(EffectPhase::Authorized {
            effect: effect.clone(),
            verdict: AuthorizationVerdict::Allow,
        }),
        EventPayload::Effect(EffectPhase::Dispatched {
            effect: effect.clone(),
        }),
        EventPayload::Effect(EffectPhase::Outcome {
            effect: effect.clone(),
            outcome: EffectOutcome::Ok,
            freshness: None,
            workspace_mutation: Some(WorkspaceMutation {
                effect_id: effect,
                mutation_digest: checkpoint.post_digest.clone(),
                workspace_revision: None,
                subject_digest: None,
            }),
        }),
        EventPayload::CheckpointRecorded(checkpoint),
    ];
    let mut events: Vec<_> = payloads
        .into_iter()
        .enumerate()
        .map(|(ordinal, payload)| EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(format!("seed-checkpoint-{label}-{ordinal}")),
            seq: 0,
            session_id: session.clone(),
            branch_id: None,
            run_id: Some(run.clone()),
            agent_id: None,
            device_id: DeviceId::new("synthetic-device"),
            authority_epoch: 0,
            worker_generation: store.worker_generation(),
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: RawPayload::from_event(payload).expect("checkpoint payload"),
        })
        .collect();
    store
        .append(&mut events)
        .await
        .expect("recovered checkpoint journal");
    assert_eq!(
        store
            .list_checkpoints(session.clone(), None, None, 10)
            .await
            .expect("indexed checkpoint")
            .checkpoints
            .len(),
        1
    );
    drop(cas);
    store.close().await.expect("close seed");
    session
}

async fn exercise_recovered_checkpoint_rpc(
    task: &DaemonTask,
    cases: &[(haider_protocol::ids::SessionId, std::path::PathBuf, bool)],
) {
    let metadata = task.diagnostics().snapshot().bootstrap.expect("metadata");
    for (session, target, allowed) in cases {
        let mut client = Client::new(&metadata.endpoint_path).await;
        assert!(matches!(client.hello(1).await, WireFrame::Welcome(_)));
        let attached = client.request(json!({"method":"session.attach","session_id":session,"after_seq":0,"mode":"control"})).await;
        let ResponseBody::SessionAttach { attach_state, .. } = attached else {
            panic!("attach recovered checkpoint session: {attached:?}");
        };
        let worker_generation = attach_state.worker_generation;
        assert_ne!(
            worker_generation, metadata.daemon_generation,
            "out-of-band seed opens must distinguish worker and daemon generations"
        );
        let undo = client.request(json!({"method":"checkpoint.undo", "command_id":format!("undo-{}", session.as_str()),
            "session_id":session, "worker_generation":worker_generation, "target":"last"})).await;
        if *allowed {
            assert!(
                matches!(undo, ResponseBody::CheckpointUndo { .. }),
                "{undo:?}"
            );
            assert_eq!(
                std::fs::read(target).expect("undo restores within ceiling"),
                b"synthetic recovered deletion"
            );
            let redo = client.request(json!({"method":"checkpoint.redo", "command_id":format!("redo-{}", session.as_str()),
                "session_id":session, "worker_generation":worker_generation, "target":"last"})).await;
            assert!(
                matches!(redo, ResponseBody::CheckpointRedo { .. }),
                "{redo:?}"
            );
            assert!(!target.exists());
        } else {
            // Distinguish ceiling/operand refusal from an earlier permission or
            // missing-checkpoint rejection, which would not exercise this fix.
            assert!(
                matches!(undo, ResponseBody::Error { ref code, .. } if code == "invalid_argument"),
                "{undo:?}"
            );
            assert!(
                !target.exists(),
                "recovered full-trust session escaped workspace"
            );
        }
    }
}
