#![allow(clippy::expect_used)]

use crate::connection::{ConnectionContext, DrainNotice, serve};
use crate::delegation::DelegationHandle;
use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use async_trait::async_trait;
use haider_core::{
    SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    TurnAdmissionDisposition, TurnCancelCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::agent::ChipState;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::ids::{DeviceId, EventId, MenuId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::Menu;
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{
    FakeInputKind, FakeProvider, FakeStep, Provider, ProviderError, ProviderStream, TurnRequest,
};
use haider_rpc::{
    AttachMode, Capability, CapabilitySet, ClientKind, CommandId, Hello, RequestBody, RequestId,
    ResponseBody, WIRE_PROTOCOL_VERSION, WireFrame, uds_codec,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, timeout};

struct InspectingProvider {
    inner: FakeProvider,
    store: SqliteStoreHandle,
    parent_session: SessionId,
    outcome_preceded_child: AtomicBool,
}

#[async_trait]
impl Provider for InspectingProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let is_child = request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, Block::Text { text } if text.starts_with("Delegated task:"))
            })
        });
        if is_child {
            let mut cursor = 0;
            let mut spawn_terminal = false;
            let mut observed = Vec::new();
            loop {
                let page = StoreHandle::read(&self.store, &self.parent_session, cursor, 256)
                    .await
                    .expect("read parent effect journal");
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map_or(cursor, |event| event.seq);
                spawn_terminal |= page.into_iter().any(|event| {
                    observed.push(event.payload.clone());
                    serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::Effect(EffectPhase::Outcome {
                                outcome: EffectOutcome::Ok,
                                ..
                            })
                        )
                    })
                });
            }
            assert!(spawn_terminal, "child started before outcome: {observed:?}");
            self.outcome_preceded_child
                .store(spawn_terminal, Ordering::SeqCst);
        }
        self.inner.stream_turn(request).await
    }
}

struct FixedProviderFactory {
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.provider.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
}

/// MUTATION CHECK: submit the child before terminalizing AgentSpawn, keep the
/// tool effect open for the child's lifetime, skip Waiting(LocalChild), or
/// resume without the report. Expected runtime failure: the child provider
/// observes no spawn outcome, the parent state chain is wrong, or its second
/// request lacks `child report`.
#[tokio::test]
async fn production_spawn_effect_wait_and_report_chain_is_end_to_end() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let parent_session = SessionId::new("w6a-parent-session");
    let provider = Arc::new(InspectingProvider {
        inner: FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "spawn-call".into(),
                name: "spawn_subagent".into(),
                args: serde_json::json!({
                    "task": "tests",
                    "prompt": "run the focused test suite"
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "child report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::ExpectToolResult {
                call_id: "spawn-call".into(),
            },
            FakeStep::EmitText {
                text: "parent merged report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]),
        store: store.clone(),
        parent_session: parent_session.clone(),
        outcome_preceded_child: AtomicBool::new(false),
    });
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-w6a-parent".into(),
        request_digest: "create-w6a-parent-digest".into(),
        request_json: r#"{"session":"w6a-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-w6a-parent"),
        device_id: DeviceId::new("w6a-test-device"),
    })
    .await
    .expect("create parent");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-w6a-parent".into(),
            request_digest: "submit-w6a-parent-digest".into(),
            request_json: r#"{"turn":"w6a-parent"}"#.into(),
            session_id: parent_session.clone(),
            worker_generation: store.worker_generation(),
            run_id: haider_protocol::ids::RunId::new("w6a-parent-run"),
            agent_id: None,
            text: "delegate the tests".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("w6a-parent-queued"),
            user_event_id: EventId::new("w6a-parent-user"),
            active_event_id: EventId::new("w6a-parent-active"),
            device_id: DeviceId::new("w6a-test-device"),
        })
        .await
        .expect("accept parent");
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent worker");

    timeout(Duration::from_secs(10), async {
        loop {
            let events = store
                .read(&parent_session, 0, 512)
                .await
                .expect("read parent");
            if events.iter().any(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent completes");

    let requests = provider.inner.requests();
    assert!(
        provider.outcome_preceded_child.load(Ordering::SeqCst),
        "spawn outcome must commit before child provider work: {requests:?}"
    );
    assert_eq!(requests.len(), 3);
    // W6c deliberately supersedes W6a's nonrecursive assertion: children
    // retain the tool so the depth cap can return a provider-readable result.
    assert!(
        requests[1]
            .tools
            .iter()
            .any(|tool| tool.name == "spawn_subagent"),
        "W6c children may recurse through the same production tool"
    );
    assert!(requests[2].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { call_id, preview, .. }
                    if call_id == "spawn-call" && preview == "child report"
            )
        })
    }));
    let parent_events = store.read(&parent_session, 0, 512).await.expect("parent");
    let payloads = parent_events
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    let waiting = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::LocalChild
                })
            )
        })
        .expect("parent waited");
    let resumed = payloads
        .iter()
        .enumerate()
        .skip(waiting + 1)
        .find_map(|(index, payload)| {
            matches!(payload, EventPayload::RunState(RunState::Thinking)).then_some(index)
        })
        .expect("parent resumed");
    assert!(
        !payloads[waiting + 1..resumed]
            .iter()
            .any(|payload| matches!(payload, EventPayload::SessionState(_)))
    );
    assert!(payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ChildResult { report },
                ..
            }) if report.summary == "child report"
        )
    }));
    let spawned = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::AgentSpawned(manifest) => Some(manifest.clone()),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(spawned.task, "tests");
    let delegation = hub
        .delegation(spawned.agent.clone())
        .await
        .expect("delegation lookup")
        .expect("delegation row");
    let child_events = store
        .read(&delegation.child_session_id, 0, 512)
        .await
        .expect("child events");
    assert!(child_events.iter().all(|event| {
        event.run_id.as_ref() != Some(&delegation.child_run_id)
            || event.agent_id.as_ref() == Some(&spawned.agent)
    }));

    // OWNER DIRECTIVE (W6d): delegation is AUTOMATIC — the child is
    // created with writes+exec pre-allowed regardless of the parent's own
    // overrides (the parent here carries None), so a child tool call can
    // never park on a human.
    // MUTATION CHECK: inherit the parent's overrides (or None) in
    // `spawn_child`'s create — this assertion fails.
    let child_metadata = store
        .session_metadata(&delegation.child_session_id)
        .await
        .expect("child metadata read")
        .expect("child metadata present");
    let overrides = child_metadata
        .permission_overrides
        .expect("child overrides present");
    assert!(overrides.allow_writes && overrides.allow_exec);

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

fn test_cwd() -> String {
    std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned()
}

async fn accept_parent(
    hub: &SessionHub,
    session_id: &SessionId,
    run_id: &RunId,
    label: &str,
) -> haider_core::AcceptedTurn {
    hub.create_internal_session(SessionCreateCommand {
        command_id: format!("create-{label}"),
        request_digest: format!("create-{label}-digest"),
        request_json: format!(r#"{{"session":"{label}"}}"#),
        session_id: session_id.clone(),
        cwd: test_cwd(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new(format!("created-{label}")),
        device_id: DeviceId::new("w6c-test-device"),
    })
    .await
    .expect("create parent");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: format!("submit-{label}"),
        request_digest: format!("submit-{label}-digest"),
        request_json: format!(r#"{{"turn":"{label}"}}"#),
        session_id: session_id.clone(),
        worker_generation: hub.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        text: "delegate recursively".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
        queued_event_id: EventId::new(format!("queued-{label}")),
        user_event_id: EventId::new(format!("user-{label}")),
        active_event_id: EventId::new(format!("active-{label}")),
        device_id: DeviceId::new("w6c-test-device"),
    })
    .await
    .expect("accept parent")
}

async fn wait_for_state(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    expected: impl Fn(&RunState) -> bool,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(session_id, 0, 1024).await.expect("read run");
            if events.iter().any(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                    |payload| matches!(payload, EventPayload::RunState(state) if expected(&state)),
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected run state");
}

fn typed_payloads(events: &[haider_protocol::envelope::RawEnvelope]) -> Vec<EventPayload> {
    events
        .iter()
        .map(|event| serde_json::from_value(event.payload.clone()).expect("payload"))
        .collect()
}

enum ParkedChildMode {
    Complete,
    StallAfterApproval,
}

struct ParkedChildHarness {
    _root: tempfile::TempDir,
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: WorkerManager,
    parent_session: SessionId,
    child: haider_core::DelegationRecord,
    menu: Menu,
    request_seq: u64,
}

async fn start_parked_child(
    label: &str,
    mode: ParkedChildMode,
    stall_deadline: Duration,
) -> ParkedChildHarness {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: format!("{label}-spawn"),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"permission","prompt":"run one command"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // W6d owner directive: children are AUTO-ALLOWED for writes/exec —
        // a permission park is unreachable. The parked-on-human laws ride
        // the still-real `request_input` (InputRequired) park instead.
        FakeStep::EmitRequestInput {
            call_id: format!("{label}-ask"),
            kind: FakeInputKind::Question,
            title: "which value?".into(),
            body: Vec::new(),
            options: Vec::new(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: format!("{label}-ask"),
        },
    ];
    match mode {
        ParkedChildMode::Complete => script.extend([
            FakeStep::EmitText {
                text: "child continued after permission".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]),
        ParkedChildMode::StallAfterApproval => script.push(FakeStep::Hang),
    }
    script.extend([
        FakeStep::ExpectToolResult {
            call_id: format!("{label}-spawn"),
        },
        FakeStep::EmitText {
            text: "parent collected permission child".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory { provider }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                stall_deadline,
            )),
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new(format!("w6d-{label}-parent"));
    let parent_run = RunId::new(format!("w6d-{label}-parent-run"));
    let accepted = accept_parent(&hub, &parent_session, &parent_run, label).await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit permission parent");
    wait_for_state(&store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("permission delegation")
        .pop()
        .expect("permission child");
    wait_for_state(&store, &child.child_session_id, |state| {
        matches!(state, RunState::InputRequired { .. })
    })
    .await;
    let events = store
        .read(&child.child_session_id, 0, 1024)
        .await
        .expect("permission child events");
    let permission_menu = events
        .iter()
        .find_map(|envelope| {
            let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
            match payload {
                EventPayload::MenuOpened(menu)
                    if matches!(menu.kind, haider_protocol::menu::MenuKind::Question) =>
                {
                    Some((menu, envelope.seq))
                }
                _ => None,
            }
        })
        .expect("permission menu and opening sequence");
    ParkedChildHarness {
        _root: root,
        store,
        hub,
        manager,
        parent_session,
        child,
        menu: permission_menu.0,
        request_seq: permission_menu.1,
    }
}

struct UdsControlClient {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    pending: VecDeque<WireFrame>,
    drain_sender: watch::Sender<Option<DrainNotice>>,
    serve_task: tokio::task::JoinHandle<()>,
    writer_owner: tokio::task::JoinHandle<()>,
}

impl UdsControlClient {
    async fn connect(hub: SessionHub) -> Self {
        let (server, stream) = UnixStream::pair().expect("live UDS pair");
        let (writers, mut writer_tasks) = mpsc::unbounded_channel();
        let writer_owner = tokio::spawn(async move {
            while let Some(task) = writer_tasks.recv().await {
                let _ = task.await;
            }
        });
        let context = ConnectionContext {
            profile_id: "w6d-test-profile".into(),
            instance_id: "w6d-test-instance".into(),
            daemon_generation: hub.worker_generation(),
            frame_limit: haider_rpc::DEFAULT_FRAME_LIMIT,
            outbound_queue_capacity: 64,
            outbound_queued_bytes: 4 * 1024 * 1024,
            max_connections: 4,
            handshake_timeout: Duration::from_secs(5),
            writers,
            owner_uid: rustix::process::geteuid().as_raw(),
            hub,
            endpoint_path: PathBuf::from("/tmp/w6d-child-control.sock"),
        };
        let (drain_sender, drain) = watch::channel(Option::<DrainNotice>::None);
        let serve_task = tokio::spawn(async move {
            let _ = serve(server, context, drain).await;
        });
        let mut client = Self {
            stream,
            decoder: uds_codec::Decoder::new(haider_rpc::DEFAULT_FRAME_LIMIT),
            pending: VecDeque::new(),
            drain_sender,
            serve_task,
            writer_owner,
        };
        client
            .send(WireFrame::Hello(Hello {
                protocol_min: WIRE_PROTOCOL_VERSION,
                protocol_max: WIRE_PROTOCOL_VERSION,
                client_name: "w6d-child-control".into(),
                client_version: "test".into(),
                client_instance_id: "w6d-child-control-1".into(),
                client_kind: ClientKind::Cli,
                capabilities_requested: CapabilitySet::from([
                    Capability::View,
                    Capability::Control,
                ]),
                max_receive_frame: u32::try_from(haider_rpc::DEFAULT_FRAME_LIMIT)
                    .expect("test frame limit fits u32"),
            }))
            .await;
        loop {
            if matches!(client.next().await, WireFrame::Welcome(_)) {
                break;
            }
        }
        client
    }

    async fn send(&mut self, frame: WireFrame) {
        let bytes =
            uds_codec::encode(&frame, haider_rpc::DEFAULT_FRAME_LIMIT).expect("UDS frame encodes");
        self.stream
            .write_all(&bytes)
            .await
            .expect("UDS frame writes");
    }

    async fn next(&mut self) -> WireFrame {
        if let Some(frame) = self.pending.pop_front() {
            return frame;
        }
        let mut buffer = [0_u8; 8192];
        loop {
            let read = self
                .stream
                .read(&mut buffer)
                .await
                .expect("UDS frame reads");
            assert_ne!(read, 0, "UDS server closed before the expected frame");
            let batch = self.decoder.push(&buffer[..read]);
            assert!(
                batch.error.is_none(),
                "UDS decoder error: {:?}",
                batch.error
            );
            self.pending.extend(batch.frames);
            if let Some(frame) = self.pending.pop_front() {
                return frame;
            }
        }
    }

    async fn attach_control(&mut self, session_id: SessionId) {
        let request_id = RequestId::new("w6d-child-attach");
        self.send(WireFrame::Request {
            request_id: request_id.clone(),
            body: RequestBody::SessionAttach {
                session_id,
                after_seq: 0,
                mode: AttachMode::Control,
            },
        })
        .await;
        let attachment_id = loop {
            match self.next().await {
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::SessionAttach { attachment_id, .. },
                } if observed == request_id => break attachment_id,
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::Error { code, message, .. },
                } if observed == request_id => panic!("child attach failed: {code}: {message}"),
                _ => {}
            }
        };
        loop {
            if matches!(
                self.next().await,
                WireFrame::AttachCaughtUp {
                    attachment_id: observed,
                    ..
                } if observed == attachment_id
            ) {
                return;
            }
        }
    }

    async fn answer_question(
        &mut self,
        session_id: SessionId,
        menu_id: MenuId,
        request_seq: u64,
        worker_generation: u64,
    ) {
        let request_id = RequestId::new("w6d-child-answer");
        self.send(WireFrame::MenuAnswer {
            request_id: Some(request_id.clone()),
            command_id: CommandId::new("w6d-child-answer-command"),
            session_id,
            menu_id,
            request_seq,
            worker_generation,
            // A zero-option Question menu: empty key, index 0, the typed
            // text rides `input` (the store's option-less validation arm).
            option_key: "".into(),
            option_index: 0,
            input: Some(haider_rpc::MenuInput::Text {
                text: "w6d-answer".into(),
            }),
        })
        .await;
        loop {
            match self.next().await {
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::MenuAnswer { .. },
                } if observed == request_id => return,
                WireFrame::Response {
                    request_id: observed,
                    body: ResponseBody::Error { code, message, .. },
                } if observed == request_id => {
                    panic!("child menu answer failed: {code}: {message}")
                }
                _ => {}
            }
        }
    }

    async fn close(self) {
        let Self {
            mut stream,
            drain_sender,
            serve_task,
            writer_owner,
            ..
        } = self;
        drop(drain_sender);
        let _ = stream.shutdown().await;
        drop(stream);
        timeout(Duration::from_secs(5), serve_task)
            .await
            .expect("UDS serve task stops")
            .expect("UDS serve task joins");
        timeout(Duration::from_secs(5), writer_owner)
            .await
            .expect("UDS writer owner stops")
            .expect("UDS writer owner joins");
    }
}

async fn wait_for_chip(
    store: &SqliteStoreHandle,
    session_id: &SessionId,
    agent: &haider_protocol::ids::AgentId,
    expected: ChipState,
) {
    timeout(Duration::from_secs(5), async {
        loop {
            let payloads = typed_payloads(
                &store
                    .read(session_id, 0, 1024)
                    .await
                    .expect("read parent chips"),
            );
            if payloads.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::AgentChipState { agent: observed, chip }
                        if observed == agent && *chip == expected
                )
            }) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected parent chip state");
}

/// MUTATION CHECK: drop the child's run-state mirror or map permission parks
/// to Thinking. Expected RUNTIME failure: the parent journal never carries
/// the exact PermissionRequired chip for the delegated agent.
#[tokio::test]
async fn child_permission_park_is_visible_in_the_parent_chip_journal() {
    let harness = start_parked_child(
        "permission-chip",
        ParkedChildMode::Complete,
        Duration::from_secs(30),
    )
    .await;
    wait_for_chip(
        &harness.store,
        &harness.parent_session,
        &harness.child.agent_id,
        ChipState::InputRequired,
    )
    .await;

    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: let the stall clock ignore a PermissionRequired park or
/// leave supervision disabled after the park resolves. Expected RUNTIME
/// failure: the child is nudged/cancelled before approval, or it never receives
/// exactly one nudge and cancellation after unpark.
#[tokio::test]
async fn permission_park_pauses_stall_supervision_and_unpark_rearms_it() {
    let harness = start_parked_child(
        "permission-stall",
        ParkedChildMode::StallAfterApproval,
        Duration::from_millis(35),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(160)).await;
    let parked_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.child.child_session_id, 0, 1024)
            .await
            .expect("parked child events"),
    );
    assert!(!parked_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::UserMessage { text, .. } if text == "report your status or conclude"
    )));
    assert!(
        !parked_payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled)))
    );

    let mut control = UdsControlClient::connect(harness.hub.clone()).await;
    control
        .attach_control(harness.child.child_session_id.clone())
        .await;
    control
        .answer_question(
            harness.child.child_session_id.clone(),
            harness.menu.id.clone(),
            harness.request_seq,
            harness.store.worker_generation(),
        )
        .await;
    wait_for_state(&harness.store, &harness.child.child_session_id, |state| {
        *state == RunState::Cancelled
    })
    .await;
    wait_for_state(&harness.store, &harness.parent_session, |state| {
        *state == RunState::Done
    })
    .await;
    let resumed_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.child.child_session_id, 0, 1024)
            .await
            .expect("resumed child events"),
    );
    assert_eq!(
        resumed_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1
    );

    control.close().await;
    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: refuse Control attach to a normal child session, route the
/// answer outside the live UDS menu CAS, or fail to wake the child's parked
/// actor. Expected RUNTIME failure: attach/answer returns a typed error, the
/// child never reaches Done, or the parent never collects the child's report.
#[tokio::test]
async fn control_attach_and_menu_answer_over_uds_complete_a_child_session() {
    let harness = start_parked_child(
        "permission-uds",
        ParkedChildMode::Complete,
        Duration::from_secs(30),
    )
    .await;
    let mut control = UdsControlClient::connect(harness.hub.clone()).await;
    control
        .attach_control(harness.child.child_session_id.clone())
        .await;
    control
        .answer_question(
            harness.child.child_session_id.clone(),
            harness.menu.id.clone(),
            harness.request_seq,
            harness.store.worker_generation(),
        )
        .await;
    wait_for_state(&harness.store, &harness.child.child_session_id, |state| {
        *state == RunState::Done
    })
    .await;
    wait_for_state(&harness.store, &harness.parent_session, |state| {
        *state == RunState::Done
    })
    .await;
    let parent_payloads = typed_payloads(
        &harness
            .store
            .read(&harness.parent_session, 0, 1024)
            .await
            .expect("parent collected events"),
    );
    assert!(parent_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::AgentReport(report)
            if report.agent == harness.child.agent_id
                && report.summary == "child continued after permission"
    )));

    control.close().await;
    harness.manager.shutdown().await.expect("manager shutdown");
    harness.hub.shutdown().await.expect("hub shutdown");
    harness.store.close().await.expect("store close");
}

/// MUTATION CHECK: remove the nudge step or allow a second nudge. Expected
/// runtime failure: the exact durable nudge count is not one. MUTATION CHECK:
/// remove the grace cancellation. Expected runtime failure: the parent never
/// reaches Done with the stall-reason report.
#[tokio::test]
async fn stalled_child_is_nudged_once_cancelled_and_settles_the_parent_report() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "stall-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"stall","prompt":"wait forever"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
        FakeStep::ExpectToolResult {
            call_id: "stall-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent resumed after stall".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let delegation = DelegationHandle::with_stall_deadline(hub.clone(), Duration::from_millis(35));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(delegation),
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-stall-parent");
    let parent_run = RunId::new("w6c-stall-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-stall").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let delegations = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("delegations");
    assert_eq!(delegations.len(), 1);
    let child = &delegations[0];
    let child_events = store
        .read(&child.child_session_id, 0, 1024)
        .await
        .expect("child events");
    let child_payloads = typed_payloads(&child_events);
    assert_eq!(
        child_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1,
        "stall policy permits exactly one durable nudge"
    );
    assert!(
        child_payloads
            .iter()
            .any(|payload| { matches!(payload, EventPayload::RunState(RunState::Cancelled)) })
    );
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("parent events"),
    );
    assert!(parent_payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::AgentReport(report)
                if report.summary.contains("stalled after one nudge")
        )
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: base the deadline on spawn time instead of the newest
/// committed progress. Expected runtime failure: a nudge UserMessage appears
/// while the slow child is still emitting reasoning deltas.
#[tokio::test]
async fn committed_child_progress_resets_the_stall_deadline() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: "slow-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"slow","prompt":"make steady progress"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ];
    for index in 0..7 {
        script.push(FakeStep::Delay { ms: 12 });
        script.push(FakeStep::EmitReasoning {
            text: format!("heartbeat-{index}"),
        });
    }
    script.extend([
        FakeStep::EmitText {
            text: "slow child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "slow-spawn".into(),
        },
        FakeStep::EmitText {
            text: "slow child merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                Duration::from_millis(30),
            )),
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-progress-parent");
    let parent_run = RunId::new("w6c-progress-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-progress").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let child = hub
        .delegations_for_parent_run(parent_session, parent_run)
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let child_payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child events"),
    );
    assert!(!child_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::UserMessage { text, .. } if text == "report your status or conclude"
    )));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK (review round, W6c): make the post-nudge cancel window
/// ignore committed progress (deadline from `nudge_at_ms` alone). Expected
/// runtime failure: the recovering child below — silent long enough to
/// draw the nudge, then steadily productive — is cancelled anyway, so the
/// no-Cancelled assertion (and the child's own report reaching the
/// parent) dies. A nudge is a question, not a sentence.
#[tokio::test]
async fn a_child_that_recovers_after_the_nudge_is_never_cancelled() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let mut script = vec![
        FakeStep::EmitToolCall {
            call_id: "recover-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"recover","prompt":"pause then work"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // Silent past the 35ms deadline AND across a 25ms poll tick (the
        // supervision grid samples at 25/50/75…; a shorter silence falls
        // between polls and the nudge never fires) — then recovering
        // INSIDE the post-nudge window, which is the whole point.
        FakeStep::Delay { ms: 65 },
    ];
    for index in 0..10 {
        script.push(FakeStep::EmitReasoning {
            text: format!("recovered-heartbeat-{index}"),
        });
        script.push(FakeStep::Delay { ms: 12 });
    }
    script.extend([
        FakeStep::EmitText {
            text: "recovered child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        // The nudge landed as a MID-RUN steer: the child's run continues
        // with one more provider round to answer it before terminalizing
        // (by design — a steered turn is the same logical run).
        FakeStep::EmitText {
            text: "status acknowledged — concluding".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "recover-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged the recovered report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let delegation = DelegationHandle::with_stall_deadline(hub.clone(), Duration::from_millis(35));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(delegation),
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-recover-parent");
    let parent_run = RunId::new("w6c-recover-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-recover").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let child_payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child events"),
    );
    assert_eq!(
        child_payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1,
        "the nudge DID fire — without it this pin proves nothing"
    );
    assert!(
        !child_payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled))),
        "post-nudge progress averts the cancel"
    );
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("parent events"),
    );
    assert!(
        parent_payloads.iter().any(|payload| matches!(
            payload,
            EventPayload::Item(haider_protocol::item::ItemEvent::Completed { item, .. })
                if matches!(item, haider_protocol::item::TurnItem::AgentMessage { text }
                    if text == "parent merged the recovered report")
        )),
        "the parent merges the child's OWN report, not a stall summary"
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: remove the coordinator's cancellation sweep. Expected
/// runtime failure: the parent reaches Cancelled while its child remains
/// Streaming, tripping the child terminal-state wait below.
#[tokio::test]
async fn parent_cancel_sweeps_its_outstanding_child() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cancel-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"orphan","prompt":"hang"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-cancel-parent");
    let parent_run = RunId::new("w6c-cancel-parent-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-cancel").await;
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run.clone())
        .await
        .expect("delegations")
        .pop()
        .expect("child");
    let cancel_json = serde_json::json!({
        "session_id": parent_session,
        "run_id": parent_run,
        "reason": "test-parent-cancel",
    })
    .to_string();
    hub.cancel_internal_turn(TurnCancelCommand {
        command_id: "cancel-w6c-parent".into(),
        request_digest: blake3::hash(cancel_json.as_bytes()).to_hex().to_string(),
        request_json: cancel_json,
        session_id: parent_session.clone(),
        worker_generation: hub.worker_generation(),
        run_id: parent_run,
        cancelling_event_id: EventId::new("w6c-parent-cancelling"),
        device_id: DeviceId::new("w6c-test-device"),
    })
    .await
    .expect("cancel parent");
    wait_for_state(&store, &parent_session, |state| {
        *state == RunState::Cancelled
    })
    .await;
    wait_for_state(&store, &child.child_session_id, |state| {
        *state == RunState::Cancelled
    })
    .await;

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: reset depth to one for recursive children, remove the cap,
/// or return a dispatcher error at the cap. Expected runtime failure: the
/// ancestry chain/depths differ, a fourth delegation appears, or the root
/// turn does not reach Done after receiving the typed cap result.
#[tokio::test]
async fn recursion_chains_ancestry_and_depth_four_is_a_typed_continuable_error() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "depth-1".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-1","prompt":"spawn depth two"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-2".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-2","prompt":"spawn depth three"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-3".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-3","prompt":"test the cap"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "depth-4-rejected".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"depth-4","prompt":"must be rejected"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-4-rejected".into(),
        },
        FakeStep::EmitText {
            text: "depth three continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-3".into(),
        },
        FakeStep::EmitText {
            text: "depth two report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-2".into(),
        },
        FakeStep::EmitText {
            text: "depth one report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::ExpectToolResult {
            call_id: "depth-1".into(),
        },
        FakeStep::EmitText {
            text: "root merged recursion".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-recursion-root");
    let parent_run = RunId::new("w6c-recursion-root-run");
    let accepted = accept_parent(&hub, &parent_session, &parent_run, "w6c-recursion").await;
    manager_handle.submit(accepted).await.expect("submit root");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;

    let depth_one = hub
        .delegations_for_parent_run(parent_session.clone(), parent_run)
        .await
        .expect("depth one")
        .pop()
        .expect("depth one row");
    let depth_two = hub
        .delegations_for_parent_run(
            depth_one.child_session_id.clone(),
            depth_one.child_run_id.clone(),
        )
        .await
        .expect("depth two")
        .pop()
        .expect("depth two row");
    let depth_three = hub
        .delegations_for_parent_run(
            depth_two.child_session_id.clone(),
            depth_two.child_run_id.clone(),
        )
        .await
        .expect("depth three")
        .pop()
        .expect("depth three row");
    assert_eq!(
        (depth_one.depth, depth_two.depth, depth_three.depth),
        (1, 2, 3)
    );
    assert_eq!(depth_one.root_session_id, parent_session);
    assert_eq!(depth_two.root_session_id, parent_session);
    assert_eq!(depth_three.root_session_id, parent_session);
    assert_eq!(depth_two.parent_agent_id, Some(depth_one.agent_id.clone()));
    assert_eq!(
        depth_three.parent_agent_id,
        Some(depth_two.agent_id.clone())
    );
    assert!(
        hub.delegations_for_parent_run(
            depth_three.child_session_id.clone(),
            depth_three.child_run_id.clone(),
        )
        .await
        .expect("cap children")
        .is_empty(),
        "the rejected depth-four call must not establish a delegation"
    );
    let requests = provider.requests();
    assert!(requests.iter().any(|request| {
        request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    Block::ToolResult { call_id, preview, .. }
                        if call_id == "depth-4-rejected"
                            && preview.contains("recursion_depth_limit")
                )
            })
        })
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: terminalize delegated children during startup recovery or
/// arm supervision only at spawn time. Expected runtime failure: the resumed
/// parent receives a generic restart failure (or waits forever) instead of
/// the one-nudge stall report.
#[tokio::test]
async fn coordinator_restart_mid_wait_rearms_supervision_from_durable_progress() {
    use crate::turn_recovery::{RecoveredWork, recover_interrupted_turns};

    let root = tempfile::tempdir().expect("temp profile");
    let first_store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "restart-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"restart","prompt":"remain stalled"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Hang,
        FakeStep::ExpectToolResult {
            call_id: "restart-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent resumed after daemon restart".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let first_hub = SessionHub::new(first_store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        first_hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
        },
        false,
    );
    let first_manager_handle = manager.handle();
    first_hub
        .install_worker_manager(first_manager_handle.clone())
        .expect("install manager");
    let parent_session = SessionId::new("w6c-restart-parent");
    let parent_run = RunId::new("w6c-restart-parent-run");
    let accepted = accept_parent(&first_hub, &parent_session, &parent_run, "w6c-restart").await;
    first_manager_handle
        .submit(accepted)
        .await
        .expect("submit parent");
    wait_for_state(&first_store, &parent_session, |state| {
        matches!(
            state,
            RunState::Waiting {
                reason: WaitReason::LocalChild
            }
        )
    })
    .await;
    let child = first_hub
        .delegations_for_parent_run(parent_session.clone(), parent_run.clone())
        .await
        .expect("delegation")
        .pop()
        .expect("child");

    manager.crash().await;
    first_hub.shutdown().await.expect("first hub shutdown");
    drop(first_hub);
    first_store.close().await.expect("first store close");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("restarted store");
    let recovered = recover_interrupted_turns(&store, &DeviceId::new("w6c-restart-device"))
        .await
        .expect("turn recovery");
    let child_before_resume = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("preserved child"),
    );
    assert!(
        !child_before_resume
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()))
    );

    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("restarted hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: Some(DelegationHandle::with_stall_deadline(
                hub.clone(),
                Duration::from_millis(30),
            )),
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install restarted manager");
    let mut resumed_parent = false;
    for work in recovered {
        match work {
            RecoveredWork::ChildWait(recovered) => {
                resumed_parent = true;
                manager_handle
                    .recover_child_wait(recovered.accepted, recovered.checkpoint)
                    .await
                    .expect("resume parent child wait");
            }
            RecoveredWork::Queued(accepted) => manager_handle
                .recover_queued(accepted)
                .await
                .expect("recover queued work"),
            RecoveredWork::Checkpoint(recovered) => manager_handle
                .recover_checkpoint(
                    recovered.accepted,
                    recovered.checkpoint,
                    recovered.committed_answer,
                )
                .await
                .expect("recover checkpoint"),
        }
    }
    assert!(resumed_parent, "parent child wait must survive restart");
    wait_for_state(&store, &parent_session, |state| *state == RunState::Done).await;
    let parent_payloads = typed_payloads(
        &store
            .read(&parent_session, 0, 1024)
            .await
            .expect("resumed parent"),
    );
    assert!(parent_payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::AgentReport(report) if report.summary.contains("stalled after one nudge")
    )));
    let payloads = typed_payloads(
        &store
            .read(&child.child_session_id, 0, 1024)
            .await
            .expect("child after resume"),
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::UserMessage { text, .. }
                    if text == "report your status or conclude"
            ))
            .count(),
        1
    );
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Cancelled)))
    );

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
