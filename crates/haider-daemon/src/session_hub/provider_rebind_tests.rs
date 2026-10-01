#![allow(clippy::expect_used)]

use super::*;
use crate::auto_hermetic::ProviderLockdownPolicy;

#[derive(Default)]
struct RebindCaptureSink(std::sync::Mutex<Vec<WireFrame>>);

impl FrameSink for RebindCaptureSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("capture frame").push(frame);
        Ok(())
    }
}

#[derive(Default)]
struct ActiveBudgetFactory;

#[async_trait::async_trait]
impl crate::worker::ProviderFactory for ActiveBudgetFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<crate::worker::ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(crate::worker::ResolvedTurnProvider {
            provider: Arc::new(haider_provider::FakeProvider::new(Vec::new())),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: Some(1_048_576),
            account_alias: metadata.account_alias.clone(),
            account_incarnation: Some(1),
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

/// The queued selection is deliberately different from the active run. The
/// handler must derive and persist the active record itself at run start.
#[tokio::test]
async fn rebind_rpc_records_active_run_budget_from_admission_snapshot() {
    for origin in [
        "create",
        "node-fork",
        "prompt-fork",
        "metafork",
        "branch",
        "internal-child",
    ] {
        assert_active_budget_from_origin(origin).await;
    }
}

async fn assert_active_budget_from_origin(origin: &str) {
    use haider_protocol::output_budget::{
        OutputBudgetClampV1, SessionOutputBudgetSourceV1, SessionOutputBudgetV1,
    };
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let mut session_id = SessionId::new("active-budget-rpc");
    let mut branch_id = None;
    let run_id = RunId::new("active-budget-run");
    let source = SessionOutputBudgetSourceV1::UserSet { requested: 20_000 };
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: "active-budget-create".into(),
        request_digest: "active-budget-create-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "mock-a".into(),
        model: "small".into(),
        max_tokens: 20_000,
        max_tokens_source: Some(source),
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test".into(),
        event_id: EventId::new("active-budget-created"),
        device_id: DeviceId::new("budget-device"),
    })
    .await
    .expect("create");
    if origin == "internal-child" {
        let child_id = SessionId::new("active-budget-internal-child");
        hub.create_internal_session(haider_core::SessionCreateCommand {
            command_id: "active-budget-child-create".into(),
            request_digest: "active-budget-child-create-digest".into(),
            request_json: "{}".into(),
            session_id: child_id.clone(),
            cwd: "/tmp".into(),
            provider: "mock-a".into(),
            model: "small".into(),
            max_tokens: 20_000,
            max_tokens_source: Some(source),
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test".into(),
            event_id: EventId::new("active-budget-child-created"),
            device_id: DeviceId::new("budget-device"),
        })
        .await
        .expect("internal child creation");
        session_id = child_id;
    }
    if !matches!(origin, "create" | "internal-child") {
        let source_run = RunId::new("fork-source-run");
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "fork-source-accept".into(),
            request_digest: "fork-source-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: source_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "fork source prompt".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("fork-source-queued"),
            user_event_id: EventId::new("fork-source-user"),
            active_event_id: EventId::new("fork-source-active"),
            device_id: DeviceId::new("budget-device"),
        })
        .await
        .expect("source turn");
        let events = store
            .read(&session_id, 0, 32)
            .await
            .expect("source journal");
        let (node, node_seq) = events
            .iter()
            .find_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::NodeCommitted(haider_protocol::history::TreeNode {
                    node,
                    ..
                })) if event.run_id.as_ref() == Some(&source_run) => Some((node, event.seq)),
                _ => None,
            })
            .expect("source prompt node");
        let user_seq = events
            .iter()
            .find_map(|event| match event.payload.decode_event() {
                Ok(EventPayload::UserMessage { .. })
                    if event.run_id.as_ref() == Some(&source_run) =>
                {
                    Some(event.seq)
                }
                _ => None,
            })
            .expect("source user prompt");
        let mut terminal = events
            .iter()
            .find(|event| {
                event.run_id.as_ref() == Some(&source_run)
                    && matches!(
                        event.payload.decode_event(),
                        Ok(EventPayload::RunState(RunState::Queued))
                    )
            })
            .expect("source queued")
            .clone();
        terminal.event_id = EventId::new("fork-source-done");
        terminal.seq = 0;
        terminal.payload = serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("done payload")
            .into();
        hub.append(&mut [terminal]).await.expect("source terminal");
        if origin == "branch" {
            let request_json =
                serde_json::json!({"origin":origin,"session":session_id}).to_string();
            let branch = haider_protocol::ids::BranchId::new("active-budget-branch");
            let outcome = store
                .create_branch(haider_store::BranchCreateCommand {
                    command_id: "active-budget-branch-create".into(),
                    request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation: store.worker_generation(),
                    branch_id: branch.clone(),
                    source_branch_id: None,
                    fork_node_id: node.clone(),
                    fork_seq: node_seq,
                    name: Some("budget branch".into()),
                    event_id: EventId::new("active-budget-branch-event"),
                    device_id: DeviceId::new("budget-device"),
                })
                .await
                .expect("branch creation");
            assert!(matches!(
                outcome,
                haider_store::BranchCreateOutcome::Committed { .. }
            ));
            branch_id = Some(branch);
        } else {
            let child_id = SessionId::new(format!("active-budget-{origin}"));
            let request_json =
                serde_json::json!({"origin":origin,"source":session_id,"child":child_id})
                    .to_string();
            let digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
            let created = if origin == "prompt-fork" {
                let outcome = store
                    .fork_session_from_prompt(haider_store::SessionPromptForkCommand {
                        command_id: format!("budget-{origin}-command"),
                        request_digest: digest,
                        request_json,
                        source_session_id: session_id.clone(),
                        session_id: child_id,
                        worker_generation: store.worker_generation(),
                        source_branch_id: None,
                        prompt_seq: user_seq,
                        name: Some("budget child".into()),
                        audit_event_id: EventId::new("budget-prompt-fork-audit"),
                        device_id: DeviceId::new("budget-device"),
                    })
                    .await
                    .expect("prompt fork");
                match outcome {
                    haider_store::SessionForkOutcome::Committed { created, .. } => created,
                    _ => panic!("new prompt fork"),
                }
            } else {
                let mut metafork =
                    (origin == "metafork").then(|| haider_store::SessionMetaforkCommit {
                        description: "omit copied prompt".into(),
                        model_proposal: haider_protocol::session_fork::SessionMetaforkProposal {
                            removals: vec![haider_protocol::session_fork::SessionMetaforkRemoval {
                                from_seq: user_seq,
                                through_seq: user_seq,
                                reason: "omit prompt".into(),
                                preview: None,
                                reviewed_events: Vec::new(),
                            }],
                        },
                        accepted_proposal_digest: String::new(),
                    });
                if let Some(commit) = metafork.as_mut() {
                    commit.accepted_proposal_digest =
                        haider_protocol::session_fork::SessionMetaforkReviewManifest {
                            command_id: format!("budget-{origin}-command"),
                            source_session_id: session_id.clone(),
                            worker_generation: store.worker_generation(),
                            source_branch_id: None,
                            fork_node_id: node.clone(),
                            fork_seq: node_seq,
                            name: Some("budget child".into()),
                            description: commit.description.clone(),
                            model_proposal: commit.model_proposal.clone(),
                        }
                        .digest()
                        .expect("review digest");
                }
                let request_json =
                    serde_json::json!({"origin":origin,"source":session_id,"child":child_id,
                "metafork":metafork})
                    .to_string();
                let outcome = store
                    .fork_session(haider_store::SessionForkCommand {
                        command_id: format!("budget-{origin}-command"),
                        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                        request_json,
                        source_session_id: session_id.clone(),
                        session_id: child_id,
                        worker_generation: store.worker_generation(),
                        source_branch_id: None,
                        fork_node_id: node,
                        fork_seq: node_seq,
                        name: Some("budget child".into()),
                        metafork,
                        audit_event_id: EventId::new(format!("budget-{origin}-audit")),
                        device_id: DeviceId::new("budget-device"),
                    })
                    .await
                    .expect("fork");
                match outcome {
                    haider_store::SessionForkOutcome::Committed { created, .. } => created,
                    _ => panic!("new fork"),
                }
            };
            session_id = created.session_id;
        }
    }
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: "active-budget-accept".into(),
        request_digest: "active-budget-accept-digest".into(),
        request_json: serde_json::json!({"headless": {
            "provider":"mock-a", "model":"small", "max_output_tokens":20000,
            "fast":false, "permission_overrides":{}, "budget":{}
        }})
        .to_string(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: branch_id.clone(),
        text: "active".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new("active-budget-queued"),
        user_event_id: EventId::new("active-budget-user"),
        active_event_id: EventId::new("active-budget-active"),
        device_id: DeviceId::new("budget-device"),
    })
    .await
    .expect("accept");
    hub.bind_lockdown_turn(&session_id, &run_id, "mock-a", ProviderLockdownPolicy::Full)
        .expect("bind");
    hub.activate_lockdown_turn(&session_id, &run_id)
        .expect("activate");
    hub.select_session_model(haider_store::SessionSelectModelCommand {
        command_id: "active-budget-queued-select".into(),
        request_digest: "active-budget-select-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        provider: "mock-a".into(),
        model: "wide".into(),
        expected_pair: None,
        output_budget: Some(SessionOutputBudgetV1 {
            max_tokens: 6_000,
            source: SessionOutputBudgetSourceV1::UserSet { requested: 6_000 },
            clamped: None,
        }),
        event_id: EventId::new("active-budget-selected"),
        device_id: DeviceId::new("budget-device"),
    })
    .await
    .expect("queue next model");
    let mut started = store
        .read(&session_id, 0, 16)
        .await
        .expect("accepted journal")
        .into_iter()
        .find(|event| {
            event.run_id.as_ref() == Some(&run_id)
                && matches!(
                    event.payload.decode_event(),
                    Ok(EventPayload::RunState(RunState::Queued))
                )
        })
        .expect("queued run");
    started.event_id = EventId::new("active-budget-thinking");
    started.seq = 0;
    started.payload = serde_json::to_value(EventPayload::RunState(RunState::Thinking))
        .expect("thinking payload")
        .into();
    hub.append(&mut [started])
        .await
        .expect("run starts after queued selection");
    let profile = |provider: &str| {
        serde_json::from_value::<haider_rpc::ProviderSummaryWire>(serde_json::json!({
        "provider": provider, "api_family":"openai_chat_completions", "availability":"available",
        "enabled":true, "trust":"full", "inventory":{"state":"static"},
        "models":["small","wide"], "default_model":"small",
        "model_details":[
            {"name":"small","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
            {"name":"wide","max_output_tokens":30000,"max_output_tokens_source":"declared","context_window":1048576}
        ]
    })).expect("profile")
    };
    let descriptor = haider_protocol::credential::CredentialDescriptor {
        alias: haider_protocol::ids::CredentialAlias::new("budget-account"),
        provider: "mock-b".into(),
        base_url: None,
        auth_method: haider_protocol::credential::AuthMethod::ApiKey,
        identity: "test account".into(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        active: true,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(std::sync::Mutex::new(vec![descriptor.clone()])),
        management: crate::accounts::ManagementSnapshot::new(
            1,
            vec![descriptor],
            vec![profile("mock-a"), profile("mock-b")],
        ),
        vault_supported: false,
        discovery_disabled: true,
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
        vault: None,
    })
    .expect("accounts");
    hub.install_creatable_providers(std::collections::BTreeSet::from([
        "mock-a".into(),
        "mock-b".into(),
    ]))
    .expect("providers");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let before_rebind = lease
        .session_metadata()
        .await
        .expect("metadata")
        .expect("typed");
    let resolver = crate::worker::provider_rebind::DaemonProviderRebindResolver::new(
        lease.clone(),
        run_id.clone(),
        Arc::new(ActiveBudgetFactory),
        before_rebind,
        crate::worker::WebCapabilityDegrade::default(),
    );
    let sink = Arc::new(RebindCaptureSink::default());
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
            RequestId::new("attach-active-budget"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach");
    connection
        .request(
            RequestId::new("rebind-active-budget"),
            RequestBody::SessionProviderRebind {
                command_id: haider_rpc::CommandId::new("rebind-active-budget-command"),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                provider: "mock-b".into(),
                base_url: None,
                account: Some("budget-account".into()),
            },
        )
        .await
        .expect("rebind");
    let active = haider_protocol::session::ActiveRunOutputBudgetV1 {
        run_id,
        model: "small".into(),
        budget: SessionOutputBudgetV1 {
            max_tokens: 8_192,
            source,
            clamped: Some(OutputBudgetClampV1 {
                requested: 20_000,
                max_output_tokens: 8_192,
            }),
        },
    };
    let response = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response { request_id, body }
                if request_id.as_str() == "rebind-active-budget" =>
            {
                Some(body.clone())
            }
            _ => None,
        })
        .expect("rebind response");
    assert!(
        matches!(response, ResponseBody::SessionProviderRebind { active_output_budget: Some(ref recorded), .. }
        if recorded == &active),
        "computed response: {response:?}"
    );
    let fact = store
        .read(&session_id, 0, 128)
        .await
        .expect("journal")
        .into_iter()
        .find_map(|event| {
            haider_protocol::session::SessionProviderRebound::from_payload_value(&event.payload)
        })
        .expect("typed rebind fact");
    assert_eq!(fact.active_output_budget, Some(active.clone()));
    let receipt = store
        .session_provider_rebind_receipt(
            "rebind-active-budget-command".into(),
            blake3::hash(
                serde_json::json!({
                    "session_id": session_id, "worker_generation": store.worker_generation(),
                    "provider":"mock-b", "base_url":null, "account":"budget-account"
                })
                .to_string()
                .as_bytes(),
            )
            .to_hex()
            .to_string(),
            serde_json::json!({
                "session_id": session_id, "worker_generation": store.worker_generation(),
                "provider":"mock-b", "base_url":null, "account":"budget-account"
            })
            .to_string(),
        )
        .await
        .expect("receipt")
        .expect("committed receipt");
    assert_eq!(receipt.active_output_budget, Some(active));
    let live = haider_core::ProviderRebindResolver::refresh(&resolver, "small", "{}")
        .await
        .expect("live refresh")
        .expect("rebound target");
    assert_eq!(
        live.max_tokens, 8_192,
        "live request uses computed active record"
    );
    let mut recovered = hub
        .session_metadata(&session_id)
        .await
        .expect("metadata")
        .expect("typed");
    let spec: haider_protocol::headless::HeadlessRunSpecV1 =
        serde_json::from_value(serde_json::json!({
            "provider":"mock-a", "model":"small", "max_output_tokens":20_000,
            "fast":false, "permission_overrides":{}, "budget":{}
        }))
        .expect("headless spec");
    crate::worker::provider_rebind::pin_headless_turn_metadata(
        &mut recovered,
        &spec,
        &lease,
        &RunId::new("active-budget-run"),
    )
    .await
    .expect("recovery pin");
    assert_eq!(
        recovered.max_tokens, 8_192,
        "recovery uses computed active record"
    );
    drop(resolver);
    drop(lease);
    drop(connection);
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

/// Model refresh, endpoint validation and compaction provider resolution
/// may wait on the network. Admission and rebind remain available meanwhile.
#[tokio::test]
async fn slow_inventory_refresh_does_not_hold_session_selection_lock() {
    for operation in ["model", "endpoint", "compaction"] {
        assert_slow_inventory_fetch_is_unfenced(operation).await;
    }
}

async fn assert_slow_inventory_fetch_is_unfenced(operation: &str) {
    let root = tempfile::tempdir().expect("store root");
    let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
        .await
        .expect("hub");
    let session_id = SessionId::new("slow-inventory-session");
    hub.create_internal_session(haider_core::SessionCreateCommand {
        command_id: "slow-create".into(),
        request_digest: "slow-create-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "mock-a".into(),
        model: "small".into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test".into(),
        event_id: EventId::new("slow-created"),
        device_id: DeviceId::new("slow-device"),
    })
    .await
    .expect("create");
    let profile = |provider: &str, inventory: serde_json::Value| {
        serde_json::from_value::<haider_rpc::ProviderSummaryWire>(serde_json::json!({
            "provider": provider, "api_family":"openai_chat_completions", "availability":"available",
            "enabled":true, "trust":"full", "inventory": inventory,
            "models":["small","wide"], "default_model":"small",
            "model_details":[
                {"name":"small","max_output_tokens":8192,"max_output_tokens_source":"declared","context_window":1048576},
                {"name":"wide","max_output_tokens":30000,"max_output_tokens_source":"declared","context_window":1048576}
            ]
        })).expect("profile")
    };
    let mut target_profile = profile("mock-b", serde_json::json!({"state":"static"}));
    if operation == "model" {
        target_profile.models.retain(|model| model == "small");
        target_profile
            .model_details
            .retain(|model| model.name == "small");
    }
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(std::sync::Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(
            1,
            Vec::new(),
            vec![
                profile(
                    "mock-a",
                    serde_json::json!({"state":"stale","fetched_at_ms":0,"reason":"test"}),
                ),
                target_profile,
            ],
        ),
        vault_supported: false,
        discovery_disabled: true,
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
        vault: None,
    })
    .expect("accounts");
    hub.install_creatable_providers(std::collections::BTreeSet::from([
        "mock-a".into(),
        "mock-b".into(),
    ]))
    .expect("providers");
    let pause = Arc::new(ModelSelectionBudgetPause {
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    hub.inner
        .selection_network_pauses
        .lock()
        .await
        .insert(session_id.clone(), pause.clone());
    let sink = Arc::new(RebindCaptureSink::default());
    let make_connection = || {
        hub.open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection")
    };
    let select_connection = make_connection();
    let rebind_connection = make_connection();
    for (connection, suffix) in [
        (&select_connection, "select"),
        (&rebind_connection, "rebind"),
    ] {
        connection
            .request(
                RequestId::new(format!("attach-{suffix}")),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("attach");
    }
    let generation = store.worker_generation();
    let select_session = session_id.clone();
    let compaction_lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("compaction lease");
    let pending_operation = operation.to_owned();
    let select = tokio::spawn(async move {
        if pending_operation == "compaction" {
            let mut dependencies = crate::worker::WorkerDependencies::unconfigured_for_tests();
            dependencies.delegation = Some(crate::delegation::DelegationHandle::new(
                compaction_lease.hub().clone(),
            ));
            let result = crate::worker::perform_manual_compaction(
                &dependencies,
                &compaction_lease
                    .session_metadata()
                    .await
                    .expect("metadata")
                    .expect("typed"),
                &compaction_lease,
                &DeviceId::new("slow-device"),
                Arc::new(haider_core::EventIdGenerator::new("slow-compaction-events")),
                "slow-compaction-command".into(),
                generation,
                None,
            )
            .await;
            assert!(
                matches!(result, Err(ref error) if error.code == haider_protocol::error::ErrorCode::Busy),
                "admission during resolution makes fresh compaction Busy: {result:?}"
            );
            return Ok(());
        }
        let request = if pending_operation == "endpoint" {
            RequestBody::SessionProviderRebind {
                command_id: haider_rpc::CommandId::new("slow-endpoint-command"),
                session_id: select_session,
                worker_generation: generation,
                provider: "mock-a".into(),
                base_url: Some("http://127.0.0.1:1/v1".into()),
                account: None,
            }
        } else {
            RequestBody::SessionSelectModel {
                command_id: haider_rpc::CommandId::new("slow-select-command"),
                session_id: select_session,
                worker_generation: generation,
                model: "wide".into(),
                provider: None,
                confirm_new_epoch: true,
                max_tokens: None,
            }
        };
        select_connection
            .request(RequestId::new("slow-select"), request)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.reached.notified())
        .await
        .expect("refresh paused");
    let admission_fence = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        hub.lock_workflow_selection(&session_id),
    )
    .await
    .expect("turn admission fence available");
    drop(admission_fence);
    let rebound_session = session_id.clone();
    let rebind = tokio::spawn(async move {
        rebind_connection
            .request(
                RequestId::new("slow-rebind"),
                RequestBody::SessionProviderRebind {
                    command_id: haider_rpc::CommandId::new("slow-rebind-command"),
                    session_id: rebound_session,
                    worker_generation: generation,
                    provider: "mock-b".into(),
                    base_url: None,
                    account: None,
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(15), rebind)
        .await
        .expect("rebind unblocked")
        .expect("rebind task")
        .expect("rebind request");
    assert!(
        sink.0
            .lock()
            .expect("frames")
            .iter()
            .any(|frame| matches!(frame,
        WireFrame::Response { request_id, body: ResponseBody::SessionProviderRebind { active_output_budget: None, .. } }
            if request_id.as_str() == "slow-rebind")),
        "rebind committed during refresh"
    );
    if operation == "model" {
        // The provider moved during the fetch, and its newly published model
        // was absent from the speculative inventory. Only a fresh retry can
        // select it successfully against the new current provider.
        lock(&hub.inner.accounts)
            .expect("account facade")
            .as_mut()
            .expect("installed accounts")
            .management = crate::accounts::ManagementSnapshot::new(
            2,
            Vec::new(),
            vec![
                profile("mock-a", serde_json::json!({"state":"static"})),
                profile("mock-b", serde_json::json!({"state":"static"})),
            ],
        );
    }
    hub.accept_internal_turn(haider_core::TurnAcceptCommand {
        command_id: "slow-admission".into(),
        request_digest: "slow-admission-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("slow-admitted-run"),
        agent_id: None,
        branch_id: None,
        text: "admitted during refresh".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new("slow-admission-queued"),
        user_event_id: EventId::new("slow-admission-user"),
        active_event_id: EventId::new("slow-admission-active"),
        device_id: DeviceId::new("slow-device"),
    })
    .await
    .expect("turn admission during refresh");
    hub.inner
        .selection_network_pauses
        .lock()
        .await
        .remove(&session_id);
    pause.release.notify_one();
    select
        .await
        .expect("selection task")
        .expect("selection request");
    if operation == "model" {
        assert!(sink.0.lock().expect("frames").iter().any(|frame| matches!(frame,
            WireFrame::Response { request_id, body: ResponseBody::SessionSelectModel { provider, model, .. } }
                if request_id.as_str() == "slow-select" && provider == "mock-b" && model == "wide")),
            "selection retries after the provider and inventory move during fetch");
    }
    hub.shutdown().await.expect("shutdown");
    store.close().await.expect("close");
}

fn rebind_policy_profile(
    provider: &str,
    policy: ProviderLockdownPolicy,
) -> haider_rpc::ProviderSummaryWire {
    serde_json::from_value(serde_json::json!({
        "provider": provider,
        "api_family": "openai_chat_completions",
        "endpoint": "http://127.0.0.1:31701/v1",
        "models": ["test-model"],
        "default_model": "test-model",
        "auth_methods": ["api_key"],
        "availability": "available",
        "enabled": true,
        "trust": if policy.is_lockdown() {"lockdown"} else {"full"},
    }))
    .expect("provider profile")
}

/// A held nonterminal turn retains its actual bound policy after ordinary
/// select_model changes durable metadata. Using the latter permits both
/// trust widening and a cross-provider sandbox switch without a new turn.
#[tokio::test]
async fn provider_rebind_guard_uses_frozen_binding_after_nonterminal_model_selection() {
    for (index, frozen_policy, selected_policy) in [
        (
            1,
            ProviderLockdownPolicy::Full,
            ProviderLockdownPolicy::Configured,
        ),
        (
            2,
            ProviderLockdownPolicy::Configured,
            ProviderLockdownPolicy::Full,
        ),
        (
            3,
            ProviderLockdownPolicy::Configured,
            ProviderLockdownPolicy::Configured,
        ),
    ] {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new(format!("rebind-frozen-{index}"));
        let run_id = RunId::new(format!("held-run-{index}"));
        let device_id = DeviceId::new("rebind-policy-test");
        hub.create_internal_session(haider_core::SessionCreateCommand {
            command_id: "create".into(),
            request_digest: "create-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "executing-provider".into(),
            model: "test-model".into(),
            max_tokens: 4096,
            max_tokens_source: None,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test-system".into(),
            event_id: EventId::new("created"),
            device_id: device_id.clone(),
        })
        .await
        .expect("session");
        hub.accept_internal_turn(haider_core::TurnAcceptCommand {
            command_id: "accept".into(),
            request_digest: "accept-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "held active turn".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("queued"),
            user_event_id: EventId::new("user"),
            active_event_id: EventId::new("active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("held turn");
        hub.bind_lockdown_turn(&session_id, &run_id, "executing-provider", frozen_policy)
            .expect("freeze policy");
        hub.activate_lockdown_turn(&session_id, &run_id)
            .expect("activate bound policy");
        hub.select_session_model(haider_store::SessionSelectModelCommand {
            command_id: "select".into(),
            request_digest: "select-digest".into(),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            provider: "pending-provider".into(),
            model: "test-model".into(),
            expected_pair: None,
            output_budget: None,
            event_id: EventId::new("selected"),
            device_id,
        })
        .await
        .expect("pending ordinary selection");
        let selection = hub.lock_workflow_selection(&session_id).await;
        assert!(
            hub.session_has_nonterminal_runs(&session_id)
                .await
                .expect("active run")
        );
        let pending = hub
            .session_metadata(&session_id)
            .await
            .expect("metadata")
            .expect("typed");
        assert_eq!(pending.provider, "pending-provider");
        let (frozen_provider, actual_policy) = hub
            .bound_session_lockdown(&session_id)
            .expect("bound policy")
            .expect("binding");
        assert_eq!(frozen_provider, "executing-provider");
        assert_eq!(actual_policy, frozen_policy);
        assert!(!rebind_matches_frozen_policy(
            &frozen_provider,
            actual_policy,
            "pending-provider",
            selected_policy
        ));
        assert!(
            rebind_matches_frozen_policy(
                &pending.provider,
                selected_policy,
                "pending-provider",
                selected_policy
            ),
            "the mutable-metadata comparison would incorrectly admit this rebind"
        );
        drop(selection);
        let descriptor = haider_protocol::credential::CredentialDescriptor {
            alias: haider_protocol::ids::CredentialAlias::new("pending-account"),
            provider: "pending-provider".into(),
            base_url: None,
            auth_method: haider_protocol::credential::AuthMethod::ApiKey,
            identity: "test account".into(),
            status: haider_protocol::credential::CredentialStatus::Ok,
            active: true,
            label: None,
            account_identity: None,
            created_at_ms: None,
        };
        hub.install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(std::sync::Mutex::new(vec![descriptor.clone()])),
            management: crate::accounts::ManagementSnapshot::new(
                1,
                vec![descriptor],
                vec![
                    rebind_policy_profile("executing-provider", frozen_policy),
                    rebind_policy_profile("pending-provider", selected_policy),
                ],
            ),
            vault_supported: false,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            vault: None,
        })
        .expect("install registry");
        hub.install_creatable_providers(std::collections::BTreeSet::from([
            "executing-provider".into(),
            "pending-provider".into(),
        ]))
        .expect("provider authority");
        let sink = Arc::new(RebindCaptureSink::default());
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
                RequestId::new("attach"),
                RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await
            .expect("control attachment");
        let head = store
            .latest_seq(&session_id)
            .await
            .expect("head before refusal");
        // Exercise the production request dispatcher and RPC handler; this
        // fails if the handler passes mutable metadata to the safe predicate.
        connection
            .request(
                RequestId::new("rebind-frozen"),
                RequestBody::SessionProviderRebind {
                    command_id: haider_rpc::CommandId::new("rebind-command"),
                    session_id: session_id.clone(),
                    worker_generation: store.worker_generation(),
                    provider: "pending-provider".into(),
                    base_url: None,
                    account: Some("pending-account".into()),
                },
            )
            .await
            .expect("rebind request handled");
        let responses = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .filter_map(|frame| match frame {
                WireFrame::Response { request_id, body }
                    if request_id.as_str() == "rebind-frozen" =>
                {
                    Some(body.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(responses.as_slice(), [ResponseBody::Error { code, retryable: true, .. }]
            if code == haider_rpc::ERROR_CODE_BUSY),
            "expected frozen-policy Busy, got {responses:?}"
        );
        assert_eq!(
            store
                .latest_seq(&session_id)
                .await
                .expect("head after refusal"),
            head
        );
        assert_eq!(
            hub.session_metadata(&session_id)
                .await
                .expect("metadata after refusal")
                .expect("typed")
                .provider_rebind_id,
            None
        );
        drop(connection);
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}

#[test]
fn provider_rebind_frozen_policy_compares_actual_permission_bits() {
    assert!(rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Full,
        "b",
        ProviderLockdownPolicy::AutoHermeticDisabled
    ));
    assert!(rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Configured,
        "a",
        ProviderLockdownPolicy::Configured
    ));
    assert!(!rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::AutoHermetic,
        "a",
        ProviderLockdownPolicy::Configured
    ));
    assert!(!rebind_matches_frozen_policy(
        "a",
        ProviderLockdownPolicy::Configured,
        "b",
        ProviderLockdownPolicy::Configured
    ));
}

/// Pause selection after it has computed its inherited/explicit budget. The
/// concurrent rebind must wait and then compute from the committed snapshot.
/// Each row is one distinct budget/source race, including budget-only config.
#[tokio::test]
async fn pair_and_budget_writers_serialize_all_seven_snapshot_variants() {
    use haider_protocol::output_budget::SessionOutputBudgetSourceV1;
    let cases = [
        (
            "declared",
            "wide",
            "small",
            20_000,
            Some(SessionOutputBudgetSourceV1::UserSet { requested: 20_000 }),
            None,
            8192,
            SessionOutputBudgetSourceV1::UserSet { requested: 20_000 },
        ),
        (
            "context",
            "unbounded",
            "ctx",
            150_000,
            Some(SessionOutputBudgetSourceV1::UserSet { requested: 150_000 }),
            None,
            100_000,
            SessionOutputBudgetSourceV1::UserSet { requested: 150_000 },
        ),
        (
            "derived",
            "wide",
            "small",
            30_000,
            Some(SessionOutputBudgetSourceV1::Derived),
            None,
            8192,
            SessionOutputBudgetSourceV1::Derived,
        ),
        (
            "restore",
            "small",
            "wide",
            8192,
            Some(SessionOutputBudgetSourceV1::UserSet { requested: 20_000 }),
            None,
            20_000,
            SessionOutputBudgetSourceV1::UserSet { requested: 20_000 },
        ),
        (
            "latest-explicit",
            "wide",
            "wide",
            20_000,
            Some(SessionOutputBudgetSourceV1::UserSet { requested: 20_000 }),
            Some(6000),
            6000,
            SessionOutputBudgetSourceV1::UserSet { requested: 6000 },
        ),
        (
            "latest-auto",
            "wide",
            "wide",
            20_000,
            Some(SessionOutputBudgetSourceV1::UserSet { requested: 20_000 }),
            Some(0),
            30_000,
            SessionOutputBudgetSourceV1::Derived,
        ),
        (
            "derived-to-explicit",
            "wide",
            "wide",
            30_000,
            Some(SessionOutputBudgetSourceV1::Derived),
            Some(6000),
            6000,
            SessionOutputBudgetSourceV1::UserSet { requested: 6000 },
        ),
    ];
    for (label, old, new, initial_budget, initial_source, explicit, expected, expected_source) in
        cases
    {
        let root = tempfile::tempdir().expect("store root");
        let (store, hub) = crate::session_hub::open_retention_test_hub(root.path())
            .await
            .expect("hub");
        let session_id = SessionId::new(format!("budget-race-{label}"));
        hub.create_internal_session(haider_core::SessionCreateCommand {
            command_id: format!("create-{label}"),
            request_digest: format!("digest-{label}"),
            request_json: "{}".into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "mock-a".into(),
            model: old.into(),
            max_tokens: initial_budget,
            max_tokens_source: initial_source,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "test".into(),
            event_id: EventId::new(format!("created-{label}")),
            device_id: DeviceId::new("race-device"),
        })
        .await
        .expect("session");
        let profiles = ["mock-a", "mock-b"].into_iter().map(|provider| {
            serde_json::from_value::<haider_rpc::ProviderSummaryWire>(serde_json::json!({
                "provider":provider, "api_family":"openai_chat_completions", "availability":"available",
                "enabled":true, "trust":"full", "inventory":{"state":"static"},
                "models":["wide","small","ctx","unbounded"], "default_model":"wide",
                "model_details":[
                    {"name":"wide","max_output_tokens":30000,"max_output_tokens_source":"declared","context_window":1048576},
                    {"name":"small","max_output_tokens": if provider == "mock-b" {8192} else {30000},"max_output_tokens_source":"declared","context_window":1048576},
                    {"name":"ctx","context_window": if provider == "mock-b" {200000} else {1048576}},
                    {"name":"unbounded","context_window":1048576}
                ]
            })).expect("profile")
        }).collect();
        hub.install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(std::sync::Mutex::new(Vec::new())),
            management: crate::accounts::ManagementSnapshot::new(1, Vec::new(), profiles),
            vault_supported: false,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            vault: None,
        })
        .expect("accounts");
        hub.install_creatable_providers(std::collections::BTreeSet::from([
            "mock-a".into(),
            "mock-b".into(),
        ]))
        .expect("creatable");
        let pause = Arc::new(ModelSelectionBudgetPause {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        hub.inner
            .model_selection_budget_pauses
            .lock()
            .await
            .insert(session_id.clone(), pause.clone());
        let sink = Arc::new(RebindCaptureSink::default());
        let select_connection = hub
            .open_connection(
                std::collections::BTreeSet::from([
                    haider_rpc::Capability::View,
                    haider_rpc::Capability::Control,
                ]),
                sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("select connection");
        let rebind_connection = hub
            .open_connection(
                std::collections::BTreeSet::from([
                    haider_rpc::Capability::View,
                    haider_rpc::Capability::Control,
                ]),
                sink.clone(),
                crate::accounts::ConnectionTransport::LocalSameUid,
            )
            .expect("rebind connection");
        for (connection, suffix) in [
            (&select_connection, "select"),
            (&rebind_connection, "rebind"),
        ] {
            connection
                .request(
                    RequestId::new(format!("attach-{suffix}")),
                    RequestBody::SessionAttach {
                        session_id: session_id.clone(),
                        after_seq: 0,
                        mode: haider_rpc::AttachMode::Control,
                        sealed_replay: false,
                    },
                )
                .await
                .expect("attach");
        }
        let selected_session = session_id.clone();
        let generation = store.worker_generation();
        let select = tokio::spawn(async move {
            select_connection
                .request(
                    RequestId::new("select"),
                    RequestBody::SessionSelectModel {
                        command_id: haider_rpc::CommandId::new("select-command"),
                        session_id: selected_session,
                        worker_generation: generation,
                        model: new.into(),
                        provider: Some("mock-a".into()),
                        confirm_new_epoch: true,
                        max_tokens: explicit,
                    },
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), pause.reached.notified())
            .await
            .expect("selection reached budget seam");
        let rebound_session = session_id.clone();
        let mut rebind = tokio::spawn(async move {
            rebind_connection
                .request(
                    RequestId::new("rebind"),
                    RequestBody::SessionProviderRebind {
                        command_id: haider_rpc::CommandId::new("rebind-command"),
                        session_id: rebound_session,
                        worker_generation: generation,
                        provider: "mock-b".into(),
                        base_url: None,
                        account: None,
                    },
                )
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), &mut rebind)
                .await
                .is_err(),
            "rebind cannot commit across the selection snapshot"
        );
        pause.release.notify_one();
        select.await.expect("select join").expect("select handled");
        rebind.await.expect("rebind join").expect("rebind handled");
        {
            let responses = sink.0.lock().expect("responses");
            assert!(
                responses.iter().any(|frame| matches!(frame,
                WireFrame::Response { request_id, body: ResponseBody::SessionProviderRebind { .. } }
                    if request_id.as_str() == "rebind")),
                "rebind response: {responses:?}"
            );
        }
        let metadata = hub
            .session_metadata(&session_id)
            .await
            .expect("metadata")
            .expect("typed");
        assert_eq!(
            (metadata.provider.as_str(), metadata.model.as_str()),
            ("mock-b", new),
            "{label}"
        );
        assert_eq!(
            (metadata.max_tokens, metadata.max_tokens_source),
            (expected, Some(expected_source)),
            "{label}"
        );
        hub.shutdown().await.expect("shutdown");
        store.close().await.expect("close");
    }
}
