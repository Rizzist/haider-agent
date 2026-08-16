//! Private session-hub accounting tests.

#![allow(clippy::expect_used)]

use super::*;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectOutcome, EffectPhase,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::graph::{EvidenceVerdict, GraphPhase, SHIP_LOOP_TEMPLATE};
use haider_protocol::ids::{AgentId, BranchId, EventId, GraphId, ItemId, MenuId, RunId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{Menu, MenuCloseReason, MenuKind, MenuOption, MenuScope};
use haider_protocol::state::RunState;
use haider_store::{
    GraphEvidenceCommand, GraphPinCommand, SessionCreateCommand, ShellExecAcceptCommand,
    ShellExecAcceptOutcome, TurnAcceptCommand,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{Notify, mpsc, oneshot, watch};

#[cfg(unix)]
const CANCELLABLE_SHELL_COMMAND: &str = "printf started; sleep 30";
#[cfg(unix)]
const CANCELLABLE_SHELL_REQUEST_JSON: &str = r#"{"command":"printf started; sleep 30"}"#;
#[cfg(windows)]
const CANCELLABLE_SHELL_COMMAND: &str = "echo|set /p=\"started\" & ping -n 31 127.0.0.1 >nul";
#[cfg(windows)]
const CANCELLABLE_SHELL_REQUEST_JSON: &str =
    r#"{"command":"echo|set /p=\"started\" & ping -n 31 127.0.0.1 >nul"}"#;

fn provider_summary(provider: &str) -> haider_rpc::ProviderSummaryWire {
    haider_rpc::ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: haider_rpc::ProviderApiFamilyWire::Unknown,
        endpoint: None,
        models: Vec::new(),
        model_details: Vec::new(),
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Unknown,
        availability_reason: None,
        default_model: None,
        enabled: true,
    }
}

/// CG-M1 LAW: an open VERIFY obligation is graph-local state, not a session
/// park. An ordinary turn acceptance still commits its normal queued/user
/// facts and never synthesizes a graph-specific run wait state.
#[tokio::test]
async fn outstanding_verify_evidence_does_not_block_an_interactive_submit() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("graph-node-scoped-wait");
    let generation = store.worker_generation();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-graph-node-scoped-wait".into(),
        request_digest: "create-graph-node-scoped-wait-digest".into(),
        request_json: r#"{"session":"graph-node-scoped-wait"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
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
        event_id: EventId::new("created-graph-node-scoped-wait"),
        device_id: DeviceId::new("graph-test"),
    })
    .await
    .expect("create session");
    hub.pin_graph(GraphPinCommand {
        command_id: "pin-node-scoped".into(),
        request_digest: "pin-node-scoped-digest".into(),
        request_json: r#"{"template":"ship-loop"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        graph_id: GraphId::new("graph-node-scoped"),
        template: SHIP_LOOP_TEMPLATE.into(),
        device_id: DeviceId::new("graph-test"),
    })
    .await
    .expect("pin graph");
    hub.record_graph_evidence(GraphEvidenceCommand {
        command_id: "evidence-node-scoped-build".into(),
        request_digest: "evidence-node-scoped-build-digest".into(),
        request_json: r#"{"node":"BUILD","verdict":"green"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: RunId::new("build-evidence-run"),
        call_id: "build-evidence-call".into(),
        graph_id: GraphId::new("graph-node-scoped"),
        node: haider_protocol::graph::build_node(),
        verdict: EvidenceVerdict::Green,
        detail: "build passed".into(),
        slot: None,
        subject_digest: None,
        signal: None,
        workspace_mutation: None,
        child_contract: None,
        device_id: DeviceId::new("graph-test"),
    })
    .await
    .expect("BUILD evidence");
    let status = hub
        .graph_status(&session_id)
        .await
        .expect("graph status")
        .expect("graph");
    assert_eq!(status.phase, GraphPhase::Active);
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    assert_eq!(status.nodes[1].evidence.green, 0);

    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "turn-during-verify".into(),
            request_digest: "turn-during-verify-digest".into(),
            request_json: r#"{"text":"ordinary interactive followup"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: RunId::new("turn-during-verify-run"),
            agent_id: None,
            branch_id: None,
            text: "ordinary interactive followup".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("turn-during-verify-queued"),
            user_event_id: EventId::new("turn-during-verify-user"),
            active_event_id: EventId::new("turn-during-verify-active"),
            device_id: DeviceId::new("graph-test"),
        })
        .await
        .expect("interactive turn remains admissible");
    assert_eq!(accepted.run_id, RunId::new("turn-during-verify-run"));
    let events = store.read(&session_id, 0, 128).await.expect("history");
    assert!(events.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::UserMessage { ref text, .. })
                if text == "ordinary interactive followup"
        )
    }));
    let status = hub
        .graph_status(&session_id)
        .await
        .expect("graph status")
        .expect("graph");
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    assert_eq!(status.phase, GraphPhase::Active);

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
    drop(root);
}

/// MUTATION CHECK: make `StopIfQuiescent` stop first and let the deleter
/// discover the accepted run afterward. Expected RUNTIME failure: the
/// barrier reports success or the still-live actor cannot acknowledge the
/// follow-up lease command.
#[tokio::test]
async fn deletion_barrier_preserves_a_prefence_accepted_turn_and_its_actor() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("delete-barrier-session");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-delete-barrier-session".into(),
        request_digest: "create-delete-barrier-session-digest".into(),
        request_json: r#"{"session":"delete-barrier-session"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
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
        event_id: EventId::new("created-delete-barrier-session"),
        device_id: DeviceId::new("delete-barrier-device"),
    })
    .await
    .expect("create session");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("session actor");
    let (accepted, acceptance) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::AcceptTurn {
            command: TurnAcceptCommand {
                command_id: "accept-delete-barrier-turn".into(),
                request_digest: "accept-delete-barrier-turn-digest".into(),
                request_json: r#"{"turn":"delete-barrier"}"#.into(),
                session_id: session_id.clone(),
                worker_generation: store.worker_generation(),
                branch_id: None,
                run_id: RunId::new("delete-barrier-run"),
                agent_id: None,
                text: "preserve this accepted turn".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Queue,
                queued_event_id: EventId::new("delete-barrier-queued"),
                user_event_id: EventId::new("delete-barrier-user"),
                active_event_id: EventId::new("delete-barrier-active"),
                device_id: DeviceId::new("delete-barrier-device"),
            },
            completed: accepted,
        })
        .await
        .expect("queue pre-fence acceptance");
    let (completed, quiescent) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::StopIfQuiescent { completed })
        .await
        .expect("queue deletion barrier");
    acceptance
        .await
        .expect("acceptance response")
        .expect("accepted turn commits");
    assert!(!quiescent.await.expect("barrier response").expect("scan"));

    let (lease_completed, lease_ack) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::AcquireWorkerLease {
            lease_id: WorkerLeaseId("delete-barrier-lease".into()),
            cancellation_wake: None,
            completed: lease_completed,
        })
        .await
        .expect("actor remains live after refused deletion");
    lease_ack.await.expect("live actor acknowledges lease");

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// The optional `provider.list` coordinate filters the production snapshot
/// projection without probing or rebuilding provider data.
///
/// MUTATION CHECK: delete the predicate from
/// `rpc::filter_provider_summaries`. Expected runtime failure: both fixture
/// providers are returned instead of only `openai`.
#[test]
fn provider_list_filter_is_applied_to_the_owned_snapshot_projection() {
    let providers = vec![provider_summary("anthropic"), provider_summary("openai")];
    let filtered = rpc::filter_provider_summaries(providers, Some("openai"));
    assert_eq!(
        filtered
            .iter()
            .map(|summary| summary.provider.as_str())
            .collect::<Vec<_>>(),
        vec!["openai"]
    );
}

/// MUTATION CHECK: move branch receipt lookup below control-attachment or
/// generation validation. Expected RUNTIME failure: after restart, the lost
/// response cannot be replayed without reattaching or using the new worker
/// generation.
#[tokio::test]
async fn branch_create_receipt_replays_before_attachment_and_generation_validation() {
    let root = tempfile::tempdir().expect("temp store");
    let session_id = SessionId::new("branch-rpc-receipt-session");
    let run_id = RunId::new("branch-rpc-fork-run");
    let command_id = haider_rpc::CommandId::new("branch-rpc-command");
    let request_id = haider_rpc::RequestId::new("branch-rpc-first");
    let (request, original_response, original_generation) = {
        let store = SqliteStoreHandle::open(root.path())
            .await
            .expect("store opens");
        let original_generation = store.worker_generation();
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
        hub.create_internal_session(SessionCreateCommand {
            command_id: "create-branch-rpc-session".into(),
            request_digest: "create-branch-rpc-session-digest".into(),
            request_json: r#"{"session":"branch-rpc-receipt"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
                .expect("canonical cwd")
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
            event_id: EventId::new("created-branch-rpc-session"),
            device_id: DeviceId::new("branch-rpc-device"),
        })
        .await
        .expect("create session");
        hub.accept_internal_turn(TurnAcceptCommand {
            command_id: "accept-branch-rpc-fork".into(),
            request_digest: "accept-branch-rpc-fork-digest".into(),
            request_json: r#"{"turn":"branch-rpc-fork"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: original_generation,
            run_id: run_id.clone(),
            agent_id: None,
            branch_id: None,
            text: "stable fork point".into(),
            attachments: Vec::new(),
            mode: haider_protocol::DeliveryMode::Queue,
            queued_event_id: EventId::new("branch-rpc-fork-queued"),
            user_event_id: EventId::new("branch-rpc-fork-user"),
            active_event_id: EventId::new("branch-rpc-fork-active"),
            device_id: DeviceId::new("branch-rpc-device"),
        })
        .await
        .expect("accept fork turn");
        let mut done = [run_state_envelope(
            &session_id,
            &run_id,
            original_generation,
            "branch-rpc-fork-done",
            RunState::Done,
        )];
        hub.append(&mut done).await.expect("terminalize fork turn");
        let events = store
            .read(&session_id, 0, 64)
            .await
            .expect("read fork node");
        let (fork_node_id, fork_seq) = events
            .iter()
            .find_map(|event| {
                let EventPayload::NodeCommitted(node) =
                    serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
                else {
                    return None;
                };
                (event.run_id.as_ref() == Some(&run_id)).then_some((node.node, event.seq))
            })
            .expect("fork node");
        let request = haider_rpc::RequestBody::BranchCreate {
            command_id: command_id.clone(),
            session_id: session_id.clone(),
            worker_generation: original_generation,
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: Some("Receipt branch".into()),
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
            .expect("control connection");
        connection
            .request(
                haider_rpc::RequestId::new("branch-rpc-attach"),
                haider_rpc::RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                },
            )
            .await
            .expect("attach control");
        sink.0.lock().expect("frames").clear();
        connection
            .request(request_id, request.clone())
            .await
            .expect("create branch");
        let response = sink
            .0
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match frame {
                WireFrame::Response {
                    body: haider_rpc::ResponseBody::BranchCreate { .. },
                    ..
                } => Some(frame.clone()),
                _ => None,
            })
            .expect("branch response");
        drop(connection);
        hub.shutdown().await.expect("hub stops");
        store.close().await.expect("store closes");
        (request, response, original_generation)
    };

    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    assert_ne!(reopened.worker_generation(), original_generation);
    let hub = SessionHub::new(reopened.clone(), SessionHubConfig::default()).expect("reopen hub");
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
        .expect("unattached control connection");
    connection
        .request(haider_rpc::RequestId::new("branch-rpc-replay"), request)
        .await
        .expect("receipt replay");
    let replay = sink
        .0
        .lock()
        .expect("replay frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body: haider_rpc::ResponseBody::BranchCreate { .. },
                ..
            } => Some(frame.clone()),
            _ => None,
        })
        .expect("replayed branch response");
    let WireFrame::Response { body: original, .. } = original_response else {
        panic!("original branch response");
    };
    let WireFrame::Response { body: replayed, .. } = replay else {
        panic!("replayed branch response");
    };
    assert_eq!(replayed, original);

    drop(connection);
    hub.shutdown().await.expect("reopened hub stops");
    reopened.close().await.expect("reopened store closes");
}

#[derive(Default)]
struct CapturingFrameSink(Mutex<Vec<WireFrame>>);

impl FrameSink for CapturingFrameSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("capturing sink").push(frame);
        Ok(())
    }
}

/// The new refresh method requires Control and hands a correlation-owned job
/// to the bounded account actor mailbox.
///
/// MUTATION CHECK: authorize `RequestBody::ProviderModelsRefresh` with
/// `Operation::View` instead of `Operation::Control`. Expected runtime
/// failure: the view-only request enters the actor mailbox and its sink has
/// no `capability_denied` response.
/// Verified by revert on 2026-07-30.
#[tokio::test]
async fn provider_models_refresh_requires_control_and_hands_off_correlation() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let (commands, mut actor_mailbox) = mpsc::channel(2);
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: Some(commands),
        oauth: None,
        snapshot: Arc::new(Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(0, Vec::new(), Vec::new()),
        vault_supported: true,
        discovery_disabled: false,
        vault: Some(Arc::new(haider_accounts::MemoryVault::default())),
    })
    .expect("install accounts");

    let view_sink = Arc::new(CapturingFrameSink::default());
    let view = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            view_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    view.request(
        haider_rpc::RequestId::new("view-refresh"),
        haider_rpc::RequestBody::ProviderModelsRefresh {
            provider: "openai-oauth".to_owned(),
        },
    )
    .await
    .expect("view rejection");
    assert!(actor_mailbox.try_recv().is_err());
    assert!(matches!(
        view_sink.0.lock().expect("view frames").as_slice(),
        [WireFrame::Response {
            body: haider_rpc::ResponseBody::Error { code, .. },
            ..
        }] if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
    ));

    let control_sink = Arc::new(CapturingFrameSink::default());
    let control = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ]),
            control_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("control connection");
    control
        .request(
            haider_rpc::RequestId::new("control-refresh"),
            haider_rpc::RequestBody::ProviderModelsRefresh {
                provider: "openai-oauth".to_owned(),
            },
        )
        .await
        .expect("control handoff");
    let command = actor_mailbox.recv().await.expect("owned actor job");
    let crate::accounts::AccountCommand::RefreshProviderModels {
        provider,
        completed,
    } = command
    else {
        panic!("unexpected actor command");
    };
    assert_eq!(provider, "openai-oauth");
    completed
        .sink
        .try_send(WireFrame::Response {
            request_id: completed.request_id,
            body: haider_rpc::ResponseBody::ProviderModelsRefresh {
                provider: provider_summary("openai-oauth"),
                revision: 4,
            },
        })
        .expect("correlated response");
    assert!(matches!(
        control_sink.0.lock().expect("control frames").as_slice(),
        [WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::ProviderModelsRefresh { revision: 4, .. },
        }] if request_id.as_str() == "control-refresh"
    ));

    drop(view);
    drop(control);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

fn run_state_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    state: RunState,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: DeviceId::new("terminal-truth-test"),
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
        payload: serde_json::to_value(EventPayload::RunState(state)).expect("state serializes"),
    }
}

fn run_payload_envelope(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    event_id: &str,
    payload: EventPayload,
) -> RawEnvelope {
    let mut envelope =
        run_state_envelope(session_id, run_id, generation, event_id, RunState::Queued);
    envelope.payload = serde_json::to_value(payload).expect("payload serializes");
    envelope
}

fn create_command(session_id: &SessionId, suffix: &str) -> SessionCreateCommand {
    #[cfg(unix)]
    let cwd = "/tmp".into();
    #[cfg(windows)]
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();

    SessionCreateCommand {
        command_id: format!("create-{suffix}"),
        request_digest: format!("create-digest-{suffix}"),
        request_json: format!(r#"{{"fixture":"{suffix}"}}"#),
        session_id: session_id.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4_096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new(format!("created-{suffix}")),
        device_id: DeviceId::new("worker-law-test"),
    }
}

fn accept_command(
    session_id: &SessionId,
    run_id: &RunId,
    generation: u64,
    suffix: &str,
) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: format!("accept-{suffix}"),
        request_digest: format!("accept-digest-{suffix}"),
        request_json: format!(r#"{{"fixture":"{suffix}"}}"#),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run_id.clone(),
        agent_id: None,
        branch_id: None,
        text: "fixture turn".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{suffix}")),
        user_event_id: EventId::new(format!("user-{suffix}")),
        active_event_id: EventId::new(format!("active-{suffix}")),
        device_id: DeviceId::new("worker-law-test"),
    }
}

/// Exact P1-3 schedule: actor FIFO commits CancelTurn first, then receives an
/// already-queued worker Done. The durable transition gate must reject Done
/// and allow only the cancellation terminal.
///
/// MUTATION CHECK: route `ActorCommand::WorkerAppend` through ordinary
/// `store.append` instead of `append_worker`. Expected failure: Done commits.
/// Verified by revert in W3c1.1.
#[tokio::test]
async fn cancelling_committed_before_worker_done_rejects_done() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("cancel-before-done");
    let run_id = RunId::new("cancel-before-done-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "cancel-before-done-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");
    let (cancel_completed, cancel_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::CancelTurn {
            command: TurnCancelCommand {
                command_id: "cancel-before-done-command".into(),
                request_digest: "cancel-before-done-digest".into(),
                request_json: "{}".into(),
                session_id: session_id.clone(),
                worker_generation: generation,
                run_id: run_id.clone(),
                cancelling_event_id: EventId::new("cancel-before-done-cancelling"),
                device_id: DeviceId::new("terminal-truth-test"),
            },
            completed: cancel_completed,
        })
        .await
        .expect("cancel queues");
    let (done_completed, done_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: None,
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "cancel-before-done-done",
                RunState::Done,
            )],
            completed: done_completed,
        })
        .await
        .expect("done queues behind cancel");

    assert!(matches!(
        cancel_response.await.expect("cancel response"),
        Ok(TurnCancelOutcome::Committed {
            envelope: Some(_),
            ..
        })
    ));
    let error = done_response
        .await
        .expect("done response")
        .expect_err("Done is rejected");
    assert_eq!(error.code, ErrorCode::RunNotActive);

    let mut cancelled = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "cancel-before-done-cancelled",
        RunState::Cancelled,
    )];
    StoreHandle::append(&lease, &mut cancelled)
        .await
        .expect("Cancelled commits");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    let states = history
        .into_iter()
        .filter_map(|envelope| serde_json::from_value::<EventPayload>(envelope.payload).ok())
        .filter_map(|payload| match payload {
            EventPayload::RunState(state) => Some(state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![RunState::Queued, RunState::Cancelling, RunState::Cancelled]
    );
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: ignore `expected_head` in the worker append arm. Expected
/// runtime failure: the stale batch commits after the intervening append,
/// which would let a compaction node fork from an obsolete tree parent.
#[tokio::test]
async fn worker_head_cas_rejects_a_compaction_batch_after_history_advances() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("compaction-head-cas");
    let run_id = RunId::new("compaction-head-cas-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "compaction-cas-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let expected_head = store.latest_seq(&session_id).await.expect("head");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");

    let (advance_completed, advance_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Append {
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "compaction-cas-advance",
                RunState::Thinking,
            )],
            completed: advance_completed,
        })
        .await
        .expect("advance queues");
    let (cas_completed, cas_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: Some(expected_head),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "compaction-cas-stale",
                RunState::Streaming,
            )],
            completed: cas_completed,
        })
        .await
        .expect("CAS queues behind advance");

    advance_response
        .await
        .expect("advance response")
        .expect("advance commits");
    let error = cas_response
        .await
        .expect("CAS response")
        .expect_err("stale compaction append is rejected");
    assert_eq!(error.code, ErrorCode::Busy);
    let history = store.read(&session_id, 0, 16).await.expect("history");
    assert!(
        !history
            .iter()
            .any(|event| { event.event_id == EventId::new("compaction-cas-stale") })
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// LAW (F3, the tolerance half of the compaction head CAS): a delta made
/// ONLY of session-config facts (`model_selected`) between the compaction's
/// planned head and the actor's head does NOT reject the batch — the fact
/// moved the journal, not the conversation tree, so the planned parent is
/// still valid and the compaction commits. The reject pin above stays the
/// other half: a run-state advance still gets the honest Busy.
///
/// MUTATION CHECK: revert the `session_config_only_delta` tolerance in the
/// actor CAS. Expected runtime failure: this append is refused Busy. The
/// inverse mutation (classifier accepts every payload) is killed by the
/// reject pin above.
#[tokio::test]
async fn worker_head_cas_tolerates_a_config_fact_delta() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("compaction-head-cas-config");
    let run_id = RunId::new("compaction-head-cas-config-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "config-cas-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let expected_head = store.latest_seq(&session_id).await.expect("head");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");

    // The interleaved journal movement is a pure config fact: no run, no
    // conversation-tree movement.
    let mut fact = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "config-cas-model-selected",
        RunState::Queued,
    );
    fact.run_id = None;
    fact.payload = haider_protocol::session::ModelSelected {
        provider: "fake-b".into(),
        model: "model-b".into(),
    }
    .to_payload_value()
    .expect("fact serializes");
    let (advance_completed, advance_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Append {
            envelopes: vec![fact],
            completed: advance_completed,
        })
        .await
        .expect("advance queues");
    let (cas_completed, cas_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: Some(expected_head),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "config-cas-batch",
                RunState::Streaming,
            )],
            completed: cas_completed,
        })
        .await
        .expect("CAS queues behind advance");

    advance_response
        .await
        .expect("advance response")
        .expect("advance commits");
    cas_response
        .await
        .expect("CAS response")
        .expect("a config-fact-only delta must not reject the batch");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    assert!(
        history
            .iter()
            .any(|event| event.event_id == EventId::new("config-cas-batch")),
        "the tolerated batch is durably committed"
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// LAW (LB4, the G2 extension of the F3 tolerance law above): a delta made
/// only of `session_renamed` config facts between the compaction's planned
/// head and the actor's head does NOT reject the batch — a rename
/// mid-compaction moves the journal, not the conversation tree, so the
/// planned parent is still valid and the compaction commits instead of
/// wedging.
///
/// MUTATION CHECK: narrow the `session_config_only_delta` classifier to
/// `model_selected` only (decode `ModelSelected` instead of the whole
/// `SessionConfigEventPayload` union). Expected runtime failure: this
/// append is refused Busy. The reject pin above still kills the inverse
/// (accept-everything) mutation.
#[tokio::test]
async fn worker_head_cas_tolerates_a_rename_fact_delta() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("compaction-head-cas-rename");
    let run_id = RunId::new("compaction-head-cas-rename-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "rename-cas-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let expected_head = store.latest_seq(&session_id).await.expect("head");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");

    // The interleaved journal movement is a pure rename fact: no run, no
    // conversation-tree movement.
    let mut fact = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "rename-cas-session-renamed",
        RunState::Queued,
    );
    fact.run_id = None;
    fact.payload = haider_protocol::session::SessionConfigEventPayload::session_renamed_value(
        Some("parser rewrite".into()),
    )
    .expect("fact serializes");
    let (advance_completed, advance_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Append {
            envelopes: vec![fact],
            completed: advance_completed,
        })
        .await
        .expect("advance queues");
    let (cas_completed, cas_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: Some(expected_head),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "rename-cas-batch",
                RunState::Streaming,
            )],
            completed: cas_completed,
        })
        .await
        .expect("CAS queues behind advance");

    advance_response
        .await
        .expect("advance response")
        .expect("advance commits");
    cas_response
        .await
        .expect("CAS response")
        .expect("a rename-fact-only delta must not reject the batch");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    assert!(
        history
            .iter()
            .any(|event| event.event_id == EventId::new("rename-cas-batch")),
        "the tolerated batch is durably committed"
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// CG-M1 LAW: graph lifecycle facts advance only the session journal, never
/// the conversation tree. A pin interleaving a planned compaction therefore
/// preserves the compaction parent just like a session-config fact does.
#[tokio::test]
async fn worker_head_cas_tolerates_a_graph_fact_delta() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("compaction-head-cas-graph");
    let run_id = RunId::new("compaction-head-cas-graph-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "graph-cas-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let expected_head = store.latest_seq(&session_id).await.expect("head");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");

    let mut graph_fact = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "graph-cas-pinned",
        RunState::Queued,
    );
    graph_fact.run_id = None;
    graph_fact.payload = serde_json::to_value(EventPayload::GraphPinned(
        haider_protocol::graph::GraphPinned {
            graph_id: GraphId::new("graph-cas-instance"),
            template: SHIP_LOOP_TEMPLATE.into(),
            digest: haider_protocol::graph::ship_loop_digest(),
            template_version: 0,
            start_node: None,
            nodes: haider_protocol::graph::ship_loop_nodes(),
        },
    ))
    .expect("graph fact serializes");
    let (advance_completed, advance_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Append {
            envelopes: vec![graph_fact],
            completed: advance_completed,
        })
        .await
        .expect("advance queues");
    let (cas_completed, cas_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: Some(expected_head),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "graph-cas-batch",
                RunState::Streaming,
            )],
            completed: cas_completed,
        })
        .await
        .expect("CAS queues behind graph fact");

    advance_response
        .await
        .expect("advance response")
        .expect("graph fact commits");
    cas_response
        .await
        .expect("CAS response")
        .expect("a graph-fact-only delta must not reject the batch");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    assert!(
        history
            .iter()
            .any(|event| event.event_id == EventId::new("graph-cas-batch"))
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// LAW (LE5, extending the F3 tolerance half above to G3's config facts): a
/// delta made of `effort_selected` AND `fast_mode_selected` facts between
/// the compaction's planned head and the actor's head does NOT reject the
/// batch — a mid-compaction `/effort` or `/fast` change must never wedge the
/// head CAS. Membership is structural: the classifier decodes the
/// `SessionConfigEventPayload` union the new variants joined.
///
/// MUTATION CHECK: remove `EffortSelected`/`FastModeSelected` from
/// `SessionConfigEventPayload` (or decode `model_selected` only in
/// `session_config_only_delta`). Expected runtime failure: this append is
/// refused Busy. The inverse (classifier accepts every payload) stays killed
/// by the reject pin above.
#[tokio::test]
async fn worker_head_cas_tolerates_an_effort_and_fast_fact_delta() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("compaction-head-cas-tuning");
    let run_id = RunId::new("compaction-head-cas-tuning-run");
    let generation = store.worker_generation();
    let mut queued = [run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "tuning-cas-queued",
        RunState::Queued,
    )];
    hub.append(&mut queued).await.expect("queued prefix");
    let expected_head = store.latest_seq(&session_id).await.expect("head");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("lease");
    let actor = hub
        .existing_actor(&session_id)
        .expect("actor lookup")
        .expect("actor exists");

    // The interleaved journal movement is BOTH G3 tuning facts: no run, no
    // conversation-tree movement.
    let mut effort_fact = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "tuning-cas-effort-selected",
        RunState::Queued,
    );
    effort_fact.run_id = None;
    effort_fact.payload = haider_protocol::session::EffortSelected {
        effort: Some("xhigh".into()),
    }
    .to_payload_value()
    .expect("effort fact serializes");
    let mut fast_fact = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "tuning-cas-fast-selected",
        RunState::Queued,
    );
    fast_fact.run_id = None;
    fast_fact.payload = haider_protocol::session::FastModeSelected { enabled: true }
        .to_payload_value()
        .expect("fast fact serializes");
    let (advance_completed, advance_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::Append {
            envelopes: vec![effort_fact, fast_fact],
            completed: advance_completed,
        })
        .await
        .expect("advance queues");
    let (cas_completed, cas_response) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::WorkerAppend {
            lease_id: lease.lease_id.clone(),
            expected_head: Some(expected_head),
            envelopes: vec![run_state_envelope(
                &session_id,
                &run_id,
                generation,
                "tuning-cas-batch",
                RunState::Streaming,
            )],
            completed: cas_completed,
        })
        .await
        .expect("CAS queues behind advance");

    advance_response
        .await
        .expect("advance response")
        .expect("advance commits");
    cas_response
        .await
        .expect("CAS response")
        .expect("an effort+fast fact delta must not reject the batch");
    let history = store.read(&session_id, 0, 16).await.expect("history");
    assert!(
        history
            .iter()
            .any(|event| event.event_id == EventId::new("tuning-cas-batch")),
        "the tolerated batch is durably committed"
    );

    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// Exact D2g handoff schedule: acceptance is durably committed, the external
/// manager gate closes and Shutdown is enqueued before the post-commit hint
/// can hand off. The hint receives typed Busy, while the drain sweep still
/// terminalizes the accepted run in this generation.
///
/// MUTATION CHECK: remove the post-supervisor durable drain sweep. Expected
/// failure: the run remains Queued after manager shutdown.
#[tokio::test]
async fn accepted_commit_then_shutdown_before_handoff_is_swept_terminal() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("drain-before-handoff");
    let run_id = RunId::new("drain-before-handoff-run");
    hub.create_session(create_command(&session_id, "drain-before-handoff"))
        .await
        .expect("typed session commits");
    let accepted = match hub
        .accept_turn(accept_command(
            &session_id,
            &run_id,
            store.worker_generation(),
            "drain-before-handoff",
        ))
        .await
        .expect("acceptance commits")
    {
        haider_store::TurnAcceptOutcome::Committed { accepted, .. }
        | haider_store::TurnAcceptOutcome::IdempotentReplay { accepted } => accepted,
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    let handle = manager.handle();
    handle.begin_draining();
    let shutdown = tokio::spawn(manager.shutdown());
    tokio::task::yield_now().await;
    let error = handle
        .submit(accepted)
        .await
        .expect_err("post-gate hint is rejected");
    assert_eq!(error.code, ErrorCode::Busy);
    shutdown
        .await
        .expect("manager task joins")
        .expect("drain sweep succeeds");

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 64)
        .await
        .expect("history reads");
    assert!(history.iter().any(|envelope| {
        envelope.run_id.as_ref() == Some(&run_id)
            && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Cancelled))
    }));
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Direct shell is still ordinary cancellable session work: a durable
/// `turn.cancel` wake must drive the existing process supervisor, settle the
/// four-phase effect, close the command item, and only then cross Cancelled.
///
/// MUTATION CHECK: await the shell process without observing cancellation,
/// or append Cancelled before broker/effect settlement. Expected runtime
/// failure: the deadline expires, the item stays open, or the ordered phase
/// and terminal assertions below fail.
#[tokio::test]
async fn direct_shell_cancellation_supervises_process_and_closes_every_lifecycle() {
    let root = tempfile::tempdir().expect("temp store");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("direct-shell-cancel");
    let run_id = RunId::new("direct-shell-cancel-run");
    let item_id = ItemId::new("direct-shell-cancel-item");
    let generation = store.worker_generation();
    let mut create = create_command(&session_id, "direct-shell-cancel");
    create.cwd = workspace.to_string_lossy().into_owned();
    hub.create_session(create)
        .await
        .expect("typed session commits");
    let accepted = match hub
        .accept_shell_exec(ShellExecAcceptCommand {
            command_id: "direct-shell-cancel-command".into(),
            request_digest: "direct-shell-cancel-digest".into(),
            request_json: CANCELLABLE_SHELL_REQUEST_JSON.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
            item_id: item_id.clone(),
            command: CANCELLABLE_SHELL_COMMAND.into(),
            running_event_id: EventId::new("direct-shell-cancel-running"),
            item_event_id: EventId::new("direct-shell-cancel-started"),
            active_event_id: EventId::new("direct-shell-cancel-active"),
            device_id: DeviceId::new("worker-law-test"),
        })
        .await
        .expect("shell acceptance commits")
    {
        ShellExecAcceptOutcome::Committed { accepted, .. }
        | ShellExecAcceptOutcome::IdempotentReplay { accepted } => accepted,
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    let handle = manager.handle();
    handle
        .shell_exec(
            accepted,
            "direct-shell-cancel-command".into(),
            CANCELLABLE_SHELL_COMMAND.into(),
            None,
        )
        .await
        .expect("shell handoff");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
                .await
                .expect("history reads");
            if history.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                        |payload| {
                            matches!(
                                payload,
                                EventPayload::Effect(EffectPhase::Dispatched { .. })
                            )
                        },
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process crosses dispatched");

    hub.cancel_turn(haider_store::TurnCancelCommand {
        command_id: "cancel-direct-shell".into(),
        request_digest: "cancel-direct-shell-digest".into(),
        request_json: r#"{"run":"direct-shell-cancel-run"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run_id.clone(),
        cancelling_event_id: EventId::new("direct-shell-cancelling"),
        device_id: DeviceId::new("worker-law-test"),
    })
    .await
    .expect("cancellation commits");

    let history = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
                .await
                .expect("history reads");
            if history.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone())
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Cancelled))
            }) {
                break history;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shell cancellation settles");
    let payloads = history
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .collect::<Vec<_>>();
    let phases = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(phases.len(), 4);
    let EffectPhase::Intent(intent) = &phases[0] else {
        panic!("effect starts with Intent");
    };
    assert!(matches!(
        &phases[1],
        EffectPhase::Authorized {
            effect,
            verdict: AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
        } if effect == &intent.effect
    ));
    assert!(matches!(
        &phases[2],
        EffectPhase::Dispatched { effect } if effect == &intent.effect
    ));
    assert!(matches!(
        &phases[3],
        EffectPhase::Outcome {
            effect,
            outcome: EffectOutcome::Cancelled | EffectOutcome::CancelledEscalated { .. },
            ..
        } if effect == &intent.effect
    ));
    let completed = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: completed,
                    item: TurnItem::CommandExecution {
                        status: ToolStatus::Cancelled,
                        ..
                    },
                }) if completed == &item_id
            )
        })
        .expect("command item closes as Cancelled");
    let terminal = payloads
        .iter()
        .position(|payload| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("run terminal");
    assert!(completed < terminal);
    assert!(!payloads.contains(&EventPayload::RunState(RunState::Done)));

    handle.begin_draining();
    manager.shutdown().await.expect("manager drains");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Graceful manager drain must wake an active direct shell without waiting
/// for its queued `SupervisorCommand::Shutdown`. The same broker-owned
/// TERM-to-KILL finalizer settles the effect and item before manager join.
///
/// MUTATION CHECK: remove the manager drain watch from `perform_shell_exec`
/// or return before broker close. Expected runtime failure: shutdown exceeds
/// the daemon's five-second drain budget or the durable lifecycle is open.
#[tokio::test]
async fn direct_shell_manager_drain_cancels_process_before_join() {
    let root = tempfile::tempdir().expect("temp store");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("direct-shell-drain");
    let run_id = RunId::new("direct-shell-drain-run");
    let item_id = ItemId::new("direct-shell-drain-item");
    let generation = store.worker_generation();
    let mut create = create_command(&session_id, "direct-shell-drain");
    create.cwd = workspace.to_string_lossy().into_owned();
    hub.create_session(create)
        .await
        .expect("typed session commits");
    let accepted = match hub
        .accept_shell_exec(ShellExecAcceptCommand {
            command_id: "direct-shell-drain-command".into(),
            request_digest: "direct-shell-drain-digest".into(),
            request_json: CANCELLABLE_SHELL_REQUEST_JSON.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: run_id.clone(),
            item_id: item_id.clone(),
            command: CANCELLABLE_SHELL_COMMAND.into(),
            running_event_id: EventId::new("direct-shell-drain-running"),
            item_event_id: EventId::new("direct-shell-drain-started"),
            active_event_id: EventId::new("direct-shell-drain-active"),
            device_id: DeviceId::new("worker-law-test"),
        })
        .await
        .expect("shell acceptance commits")
    {
        ShellExecAcceptOutcome::Committed { accepted, .. }
        | ShellExecAcceptOutcome::IdempotentReplay { accepted } => accepted,
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    manager
        .handle()
        .shell_exec(
            accepted,
            "direct-shell-drain-command".into(),
            CANCELLABLE_SHELL_COMMAND.into(),
            None,
        )
        .await
        .expect("shell handoff");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
                .await
                .expect("history reads");
            if history.iter().any(|envelope| {
                envelope.run_id.as_ref() == Some(&run_id)
                    && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                        |payload| {
                            matches!(
                                payload,
                                EventPayload::Effect(EffectPhase::Dispatched { .. })
                            )
                        },
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("process crosses dispatched");

    tokio::time::timeout(std::time::Duration::from_secs(5), manager.shutdown())
        .await
        .expect("manager drain stays within daemon deadline")
        .expect("manager drains");

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads after drain");
    let payloads = history
        .iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .collect::<Vec<_>>();
    let phases = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(phases.len(), 4);
    assert!(matches!(phases[0], EffectPhase::Intent(_)));
    assert!(matches!(phases[1], EffectPhase::Authorized { .. }));
    assert!(matches!(phases[2], EffectPhase::Dispatched { .. }));
    assert!(matches!(
        phases[3],
        EffectPhase::Outcome {
            outcome: EffectOutcome::Cancelled | EffectOutcome::CancelledEscalated { .. },
            ..
        }
    ));
    let cancelling = payloads
        .iter()
        .position(|payload| *payload == EventPayload::RunState(RunState::Cancelling))
        .expect("drain first commits Cancelling");
    let completed = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: completed,
                    item: TurnItem::CommandExecution {
                        status: ToolStatus::Cancelled,
                        ..
                    },
                }) if completed == &item_id
            )
        })
        .expect("command item closes as Cancelled");
    let terminal = payloads
        .iter()
        .position(|payload| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("run terminal");
    assert!(cancelling < completed && completed < terminal);

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Exact D1g/D3-4 interlock: a supervisor panic is observed only after
/// durable Cancelling, with one open tool item and menu. Exit terminalization
/// must use cancellation-shaped closure before Cancelled, then eviction may
/// safely permit a fresh incarnation.
///
/// MUTATION CHECK: restore the Cancelling fast-path that appends only
/// Cancelled. Expected failure: the item and menu remain open.
#[tokio::test]
async fn panic_exit_after_cancelling_closes_item_and_menu_before_cancelled() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("panic-cancelling");
    let run_id = RunId::new("panic-cancelling-run");
    let item_id = ItemId::new("panic-open-item");
    let menu_id = MenuId::new("panic-open-menu");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "panic-cancelling"))
        .await
        .expect("typed session commits");
    hub.accept_turn(accept_command(
        &session_id,
        &run_id,
        generation,
        "panic-cancelling",
    ))
    .await
    .expect("acceptance commits");
    let mut lifecycle = vec![
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "panic-item-started",
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: "panic-call".into(),
                    name: "request_input".into(),
                    args: serde_json::json!({}),
                    status: ToolStatus::InProgress,
                },
            }),
        ),
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "panic-menu-opened",
            EventPayload::MenuOpened(Menu {
                id: menu_id.clone(),
                kind: MenuKind::Choice,
                title: "Continue?".into(),
                body: Vec::new(),
                options: vec![MenuOption {
                    key: "yes".into(),
                    label: "Yes".into(),
                    detail: None,
                    decision: None,
                }],
                blocking: true,
                scope: MenuScope::Session,
                origin: "request_input".into(),
                ttl_ms: None,
                timeout_option: None,
            }),
        ),
    ];
    hub.append(&mut lifecycle)
        .await
        .expect("open lifecycle commits");
    hub.cancel_turn(haider_store::TurnCancelCommand {
        command_id: "panic-cancel-command".into(),
        request_digest: "panic-cancel-digest".into(),
        request_json: "{}".into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: run_id.clone(),
        cancelling_event_id: EventId::new("panic-cancelling-state"),
        device_id: DeviceId::new("worker-law-test"),
    })
    .await
    .expect("Cancelling commits");

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, 1)
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let completed = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: candidate,
                    item: TurnItem::ToolCall {
                        status: ToolStatus::Cancelled,
                        ..
                    },
                }) if *candidate == item_id
            )
        })
        .expect("item closes cancelled");
    let menu_closed = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::MenuClosed {
                    menu,
                    reason: MenuCloseReason::Cancelled,
                } if *menu == menu_id
            )
        })
        .expect("menu closes cancelled");
    let cancelled = payloads
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::Cancelled))
        .expect("run cancels");
    assert!(completed < cancelled && menu_closed < cancelled);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A supervisor panic with an orphaned dispatch but no cancellation must
/// reconcile Unknown and park on its recovery card instead of lying with a
/// failure-shaped terminal.
///
/// MUTATION CHECK: reconcile only the Cancelling branch. Expected failure:
/// the recovery park is absent.
#[tokio::test]
async fn panic_exit_reconciles_dispatched_and_parks_effect_unknown() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("panic-dispatched");
    let run_id = RunId::new("panic-dispatched-run");
    let effect_id = haider_protocol::ids::EffectId::new("panic-dispatched-effect");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "panic-dispatched"))
        .await
        .expect("typed session commits");
    hub.accept_turn(accept_command(
        &session_id,
        &run_id,
        generation,
        "panic-dispatched",
    ))
    .await
    .expect("acceptance commits");
    let mut dispatched = [run_payload_envelope(
        &session_id,
        &run_id,
        generation,
        "panic-effect-dispatched",
        EventPayload::Effect(haider_protocol::effect::EffectPhase::Dispatched {
            effect: effect_id.clone(),
        }),
    )];
    hub.append(&mut dispatched).await.expect("dispatch commits");

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, 1)
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload)
                .ok()
                .map(|payload| (envelope.seq, payload))
        })
        .collect::<Vec<_>>();
    let unknown = payloads
        .iter()
        .position(|(_, payload)| {
            matches!(
                payload,
                EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                    effect,
                    outcome: haider_protocol::effect::EffectOutcome::Unknown,
                    ..
                }) if *effect == effect_id
            )
        })
        .expect("Unknown reconciles");
    let parked = payloads
        .iter()
        .position(|(_, payload)| *payload == EventPayload::RunState(RunState::EffectOutcomeUnknown))
        .expect("run parks for reconciliation");
    assert!(unknown < parked);
    assert!(
        !payloads
            .iter()
            .any(|(_, payload)| *payload == EventPayload::RunState(RunState::Errored)),
        "the daemon must not overwrite genuine uncertainty with a false failure"
    );
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: remove any owned ID charge from
/// `envelope_weight_bytes` (for example `branch_id`). Expected failure: the
/// estimator falls below the explicit fixed-value-plus-owned-strings size.
#[test]
fn envelope_weight_charges_every_large_owned_id_string() {
    let large = |label: &str| format!("{label}-{}", "x".repeat(16 * 1024));
    let envelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(large("event")),
        seq: 1,
        session_id: SessionId::new(large("session")),
        branch_id: Some(BranchId::new(large("branch"))),
        run_id: Some(RunId::new(large("run"))),
        agent_id: Some(AgentId::new(large("agent"))),
        device_id: DeviceId::new(large("device")),
        authority_epoch: 2,
        worker_generation: 3,
        causation_id: Some(EventId::new(large("causation"))),
        correlation_id: Some(EventId::new(large("correlation"))),
        committed_at_ms: 4,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::Value::Null,
    };
    let owned_string_bytes = envelope
        .event_id
        .as_str()
        .len()
        .saturating_add(envelope.session_id.as_str().len())
        .saturating_add(
            envelope
                .branch_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .run_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .agent_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(envelope.device_id.as_str().len())
        .saturating_add(
            envelope
                .causation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        )
        .saturating_add(
            envelope
                .correlation_id
                .as_ref()
                .map_or(0, |value| value.as_str().len()),
        );
    let real_owned_lower_bound =
        std::mem::size_of::<RawEnvelope>().saturating_add(owned_string_bytes);

    assert!(
        envelope_weight_bytes(&envelope) >= real_owned_lower_bound,
        "every variable-length envelope field must be charged"
    );
}

struct AbortQueueSink {
    state: Mutex<AbortQueueState>,
    changed: Notify,
    pause_next_fire: AtomicBool,
    fired_reached: Notify,
    fired_release: Notify,
}

struct AbortQueueState {
    queue: VecDeque<WireFrame>,
    tickets: VecDeque<Weak<Notify>>,
}

impl AbortQueueState {
    fn prune_dead_tickets(&mut self) {
        while self
            .tickets
            .front()
            .is_some_and(|ticket| ticket.strong_count() == 0)
        {
            self.tickets.pop_front();
        }
    }

    fn ticket_is_head(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        self.tickets
            .front()
            .is_some_and(|head| Weak::ptr_eq(head, &Arc::downgrade(ticket)))
    }

    fn fire_head(&mut self) {
        self.prune_dead_tickets();
        if let Some(ticket) = self.tickets.front().and_then(Weak::upgrade) {
            ticket.notify_one();
        }
    }

    fn remove_ticket(&mut self, ticket: &AdmissionTicket) -> bool {
        self.prune_dead_tickets();
        let was_head = self.ticket_is_head(ticket);
        let token = Arc::downgrade(ticket);
        self.tickets
            .retain(|candidate| !Weak::ptr_eq(candidate, &token));
        self.prune_dead_tickets();
        was_head
    }
}

impl AbortQueueSink {
    fn new() -> Self {
        Self {
            state: Mutex::new(AbortQueueState {
                queue: VecDeque::new(),
                tickets: VecDeque::new(),
            }),
            changed: Notify::new(),
            pause_next_fire: AtomicBool::new(true),
            fired_reached: Notify::new(),
            fired_release: Notify::new(),
        }
    }

    fn offer_with_ticket(
        &self,
        frame: &WireFrame,
        ticket: Option<&AdmissionTicket>,
    ) -> SendAdmission {
        let mut state = self.state.lock().expect("abort queue state");
        state.prune_dead_tickets();
        let caller_may_admit =
            state.tickets.is_empty() || ticket.is_some_and(|ticket| state.ticket_is_head(ticket));
        if !caller_may_admit || !state.queue.is_empty() {
            return SendAdmission::Busy;
        }
        if let Some(ticket) = ticket
            && state.ticket_is_head(ticket)
        {
            state.tickets.pop_front();
        }
        state.queue.push_back(frame.clone());
        SendAdmission::Sent
    }

    async fn wait_for_tickets(&self, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let changed = self.changed.notified();
                if self.state.lock().expect("abort queue state").tickets.len() >= count {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("waiters park deterministically");
    }

    fn pop(&self) -> WireFrame {
        let mut state = self.state.lock().expect("abort queue state");
        let frame = state.queue.pop_front().expect("queued frame");
        state.fire_head();
        frame
    }
}

impl FrameSink for AbortQueueSink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
        Ok(())
    }

    fn offer(&self, _attachment_id: &AttachmentId, frame: &WireFrame) -> SendAdmission {
        self.offer_with_ticket(frame, None)
    }

    fn offer_ticketed(
        &self,
        _attachment_id: &AttachmentId,
        frame: &WireFrame,
        ticket: &AdmissionTicket,
    ) -> SendAdmission {
        self.offer_with_ticket(frame, Some(ticket))
    }

    fn drain_ticket(&self) -> Option<AdmissionTicket> {
        let ticket = Arc::new(Notify::new());
        self.state
            .lock()
            .expect("abort queue state")
            .tickets
            .push_back(Arc::downgrade(&ticket));
        self.changed.notify_waiters();
        Some(ticket)
    }

    fn cancel_ticket(&self, ticket: &AdmissionTicket) {
        let mut state = self.state.lock().expect("abort queue state");
        if state.remove_ticket(ticket) {
            state.fire_head();
        }
    }

    fn ticket_fired_test_gate(
        &self,
    ) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> {
        self.pause_next_fire
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| {
                Box::pin(async {
                    self.fired_reached.notify_one();
                    self.fired_release.notified().await;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>
            })
    }
}

fn caught_up(attachment_id: &AttachmentId, seq: u64) -> WireFrame {
    WireFrame::AttachCaughtUp {
        attachment_id: attachment_id.clone(),
        high_water_seq: seq,
    }
}

fn caught_up_seq(frame: WireFrame) -> u64 {
    let WireFrame::AttachCaughtUp { high_water_seq, .. } = frame else {
        panic!("expected caught-up frame");
    };
    high_water_seq
}

fn spawn_delivery(
    hub: SessionHub,
    sink: Arc<dyn FrameSink>,
    attachment_id: AttachmentId,
    seq: u64,
) -> tokio::task::JoinHandle<FrameDelivery> {
    tokio::spawn(async move {
        let (lag_sender, mut lagged) = watch::channel::<Option<u64>>(None);
        let (cancel_sender, mut cancel) = watch::channel(false);
        let keep_senders_alive = (lag_sender, cancel_sender);
        let result = deliver_frame(
            &hub,
            &sink,
            &attachment_id,
            &caught_up(&attachment_id, seq),
            &mut lagged,
            &mut cancel,
        )
        .await;
        drop(keep_senders_alive);
        result
    })
}

/// Capacity one: actual `deliver_frame` tasks A and B park, A's ticket fires,
/// and A is raw-aborted at the controlled fired-before-reoffer await. Fresh C
/// then joins; B must be admitted first and C after it without a wedge.
///
/// MUTATION CHECK: revert BOTH the `AdmissionTicketGuard` wiring/drop cleanup
/// and the connection outbox's dead-head successor firing. Expected failure:
/// B's timeout expires after C prunes dead A without waking the exposed head.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn aborting_deliver_frame_before_reoffer_keeps_fifo_admission_live() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let attachment_id = AttachmentId::new("abort-before-reoffer");
    let session_id = SessionId::new("abort-before-reoffer");
    let (commands, _command_receiver) = mpsc::channel(1);
    let (owner_cancel, _owner_cancel_receiver) = watch::channel(false);
    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .insert(
            attachment_id.clone(),
            AttachmentOwner {
                connection_id: "abort-test".into(),
                session_id,
                mode: AttachMode::View,
                actor: SessionActorHandle { commands },
                cancel: owner_cancel,
            },
        );

    let sink_impl = Arc::new(AbortQueueSink::new());
    let sink: Arc<dyn FrameSink> = sink_impl.clone();
    assert!(matches!(
        sink.offer(&attachment_id, &caught_up(&attachment_id, 0)),
        SendAdmission::Sent
    ));

    let first = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 1);
    sink_impl.wait_for_tickets(1).await;

    let second = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 2);
    sink_impl.wait_for_tickets(2).await;

    assert_eq!(caught_up_seq(sink_impl.pop()), 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sink_impl.fired_reached.notified(),
    )
    .await
    .expect("A reaches the fired-before-reoffer await");
    first.abort();
    let abort_error = match first.await {
        Err(error) => error,
        Ok(_) => panic!("raw abort must cancel A"),
    };
    assert!(
        abort_error.is_cancelled(),
        "A must be dropped inside deliver_frame"
    );

    let fresh = spawn_delivery(hub.clone(), Arc::clone(&sink), attachment_id.clone(), 3);

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("B is admitted after A abort")
            .expect("B task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 2);
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), fresh)
            .await
            .expect("C is admitted after B")
            .expect("C task joins"),
        FrameDelivery::Delivered
    ));
    assert_eq!(caught_up_seq(sink_impl.pop()), 3);

    lock(&hub.inner.attachments)
        .expect("attachments lock")
        .remove(&attachment_id);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// R3 aggregate-idle law at the terminalization sites (review r2 NF-1): a
/// per-run terminalization must never commit `SessionState::Idle` while any
/// other durable run in the session is nonterminal. Site under test: the
/// recovery-feed degradation path (`terminalize_recovery_feed_failure`),
/// driven through a legacy metadata-less session so `supervisor_for` fails.
/// The positive control proves the settle-guarded idle DOES commit once the
/// last nonterminal run terminalizes — the guard, not the call site, decides.
///
/// MUTATION CHECK: restore the unfiltered `failed_resumption_payloads`
/// append at `terminalize_recovery_feed_failure` (drop the SessionState
/// retain and the `append_session_idle` call). Expected failure: the
/// payload-embedded `Idle { interrupted: true }` commits while run A is
/// durably Queued, and the zero-idle assertion below fails.
#[tokio::test]
async fn recovery_terminalization_never_settles_idle_while_another_run_is_active() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("aggregate-idle-law");
    let run_a = RunId::new("aggregate-idle-run-a");
    let run_b = RunId::new("aggregate-idle-run-b");
    let generation = store.worker_generation();
    // Legacy session: raw appends only, so no typed live-worker metadata
    // exists and the recovery feed cannot build a supervisor for it.
    let mut queued = vec![
        run_state_envelope(
            &session_id,
            &run_a,
            generation,
            "idle-law-a-queued",
            RunState::Queued,
        ),
        run_state_envelope(
            &session_id,
            &run_b,
            generation,
            "idle-law-b-queued",
            RunState::Queued,
        ),
    ];
    hub.append(&mut queued)
        .await
        .expect("legacy queued runs commit");
    let accepted = |run_id: &RunId, seq: u64| haider_store::AcceptedTurn {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        accepted_seq: seq,
        worker_generation: generation,
        branch_id: None,
        disposition: haider_store::TurnAdmissionDisposition::Queued,
        first_user_turn: false,
        pdf_attachments: Vec::new(),
    };
    let manager = crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    );
    let handle = manager.handle();

    handle
        .recover_queued(accepted(&run_b, queued[1].seq))
        .await
        .expect("feed failure degrades per-item, not fatally");

    let idle_envelopes = |history: &[RawEnvelope]| {
        history
            .iter()
            .filter_map(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
            })
            .filter(|payload| {
                matches!(
                    payload,
                    EventPayload::SessionState(haider_protocol::state::SessionState::Idle { .. })
                )
            })
            .count()
    };
    let latest_state = |history: &[RawEnvelope], run: &RunId| {
        history
            .iter()
            .filter(|envelope| envelope.run_id.as_ref() == Some(run))
            .filter_map(|envelope| {
                match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                    Ok(EventPayload::RunState(state)) => Some(state),
                    _ => None,
                }
            })
            .next_back()
    };

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    assert!(
        latest_state(&history, &run_b).is_some_and(|state| state.is_terminal()),
        "run B terminalizes"
    );
    assert_eq!(
        latest_state(&history, &run_a),
        Some(RunState::Queued),
        "run A stays durably Queued"
    );
    assert_eq!(
        idle_envelopes(&history),
        0,
        "no aggregate Idle commits while run A is nonterminal"
    );

    // Positive control: terminalizing the last nonterminal run settles the
    // session — exactly one guarded Idle { interrupted: true } commits.
    handle
        .recover_queued(accepted(&run_a, queued[0].seq))
        .await
        .expect("second degradation succeeds");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history rereads");
    assert!(
        latest_state(&history, &run_a).is_some_and(|state| state.is_terminal()),
        "run A terminalizes"
    );
    assert_eq!(
        idle_envelopes(&history),
        1,
        "the settle-guarded aggregate Idle commits once the session quiesces"
    );

    handle.begin_draining();
    manager.shutdown().await.expect("manager drains");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

// ───────────────────── T1 transcription secret surface ──────────────────────

fn transcription_facade(vault: Arc<dyn haider_accounts::Vault>) -> crate::accounts::AccountsFacade {
    crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(0, Vec::new(), Vec::new()),
        vault_supported: true,
        discovery_disabled: false,
        vault: Some(vault),
    }
}

async fn transcription_request(
    connection: &HubConnection,
    sink: &CapturingFrameSink,
    request_id: &str,
    body: haider_rpc::RequestBody,
) -> haider_rpc::ResponseBody {
    connection
        .request(haider_rpc::RequestId::new(request_id), body)
        .await
        .expect("request routes");
    let mut frames = sink.0.lock().expect("frames");
    let frame = frames.pop().expect("one correlated response");
    assert!(frames.is_empty(), "exactly one response per request");
    drop(frames);
    match frame {
        WireFrame::Response {
            request_id: response_id,
            body,
        } => {
            assert_eq!(response_id.as_str(), request_id);
            body
        }
        other => panic!("expected a correlated response, got {other:?}"),
    }
}

/// The T1 secret surface end to end over the REAL FileVault: set commits a
/// physical profile-vault item under the crate alias, get returns the exact
/// key, clear removes it, and the empty state is an honest `secret: None`.
///
/// MUTATION CHECK: store the key under a per-request alias (or echo the
/// request instead of reading the vault). Expected runtime failure: the
/// physical-alias assertion below, or the fresh-connection get returns
/// nothing after a set.
#[tokio::test]
async fn transcription_secret_roundtrips_through_the_file_vault() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let vault_root = root.path().join("vault");
    let vault: Arc<dyn haider_accounts::Vault> =
        Arc::new(haider_accounts::FileVault::new(vault_root.clone()));
    hub.install_accounts(transcription_facade(Arc::clone(&vault)))
        .expect("install accounts");
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
        .expect("control connection");

    // Empty state first: an honest None, not an error.
    let body = transcription_request(
        &connection,
        &sink,
        "t1-get-empty",
        haider_rpc::RequestBody::TranscriptionSecretGet,
    )
    .await;
    assert!(matches!(
        body,
        haider_rpc::ResponseBody::TranscriptionSecretGet { secret: None }
    ));

    // Set → present: true, and the item is PHYSICALLY in the vault under
    // the crate's fixed alias.
    let body = transcription_request(
        &connection,
        &sink,
        "t1-set",
        haider_rpc::RequestBody::TranscriptionSecretSet {
            secret: haider_rpc::SecretWire::new("  dg-key-roundtrip-1f2e  "),
            clear: false,
        },
    )
    .await;
    assert!(matches!(
        body,
        haider_rpc::ResponseBody::TranscriptionSecretSet { present: true }
    ));
    let alias = super::rpc::transcription_secret_alias();
    assert_eq!(alias.as_str(), "transcription.deepgram");
    let stored = vault.resolve(&alias).expect("physical vault item");
    assert_eq!(
        stored.expose_secret(),
        b"dg-key-roundtrip-1f2e",
        "the key is stored TRIMMED under the fixed alias"
    );

    // Get returns the exact key.
    let body = transcription_request(
        &connection,
        &sink,
        "t1-get",
        haider_rpc::RequestBody::TranscriptionSecretGet,
    )
    .await;
    match body {
        haider_rpc::ResponseBody::TranscriptionSecretGet {
            secret: Some(secret),
        } => {
            assert_eq!(secret.expose_secret(), "dg-key-roundtrip-1f2e");
        }
        other => panic!("expected the stored secret, got {other:?}"),
    }

    // Clear → present: false, physical item gone, get honest-empty again.
    let body = transcription_request(
        &connection,
        &sink,
        "t1-clear",
        haider_rpc::RequestBody::TranscriptionSecretSet {
            secret: haider_rpc::SecretWire::new(""),
            clear: true,
        },
    )
    .await;
    assert!(matches!(
        body,
        haider_rpc::ResponseBody::TranscriptionSecretSet { present: false }
    ));
    assert!(vault.resolve(&alias).is_err(), "physical item removed");
    let body = transcription_request(
        &connection,
        &sink,
        "t1-get-after-clear",
        haider_rpc::RequestBody::TranscriptionSecretGet,
    )
    .await;
    assert!(matches!(
        body,
        haider_rpc::ResponseBody::TranscriptionSecretGet { secret: None }
    ));

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// UDS-ONLY LAW: the transcription secret surface answers a REMOTE
/// transport with the same-UID capability denial — for BOTH methods — and a
/// view-only local connection is denied Control.
///
/// MUTATION CHECK: route `transcription.secret_get` past
/// `secret_surface_facade` (serve it to remote transports). Expected
/// runtime failure: the remote get below receives the secret response
/// instead of `capability_denied`.
#[tokio::test]
async fn transcription_secret_surface_is_uds_and_control_only() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let vault: Arc<dyn haider_accounts::Vault> = Arc::new(haider_accounts::MemoryVault::default());
    hub.install_accounts(transcription_facade(vault))
        .expect("install accounts");

    // Remote transport with full capabilities: denied for both methods.
    let remote_sink = Arc::new(CapturingFrameSink::default());
    let remote = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::View,
                haider_rpc::Capability::Control,
            ]),
            remote_sink.clone(),
            crate::accounts::ConnectionTransport::Remote,
        )
        .expect("remote connection");
    for (index, body) in [
        haider_rpc::RequestBody::TranscriptionSecretGet,
        haider_rpc::RequestBody::TranscriptionSecretSet {
            secret: haider_rpc::SecretWire::new("dg-remote-sentinel"),
            clear: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let response =
            transcription_request(&remote, &remote_sink, &format!("t1-remote-{index}"), body).await;
        assert!(matches!(
            response,
            haider_rpc::ResponseBody::Error { ref code, ref message, .. }
                if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
                    && message.contains("same-UID")
        ));
    }

    // Local view-only connection: Control is required.
    let view_sink = Arc::new(CapturingFrameSink::default());
    let view = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            view_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    let response = transcription_request(
        &view,
        &view_sink,
        "t1-view-get",
        haider_rpc::RequestBody::TranscriptionSecretGet,
    )
    .await;
    assert!(matches!(
        response,
        haider_rpc::ResponseBody::Error { ref code, .. }
            if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
    ));

    drop(remote);
    drop(view);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// KEY-HYGIENE LAW (ADE parity): empty, oversized (>512), and
/// control-byte secrets are refused BEFORE any vault write; `clear: true`
/// with a non-empty secret is refused; and no refusal message ever echoes
/// key material.
///
/// MUTATION CHECK: validate AFTER `vault.put` (or drop the control-byte
/// check). Expected runtime failure: the vault-emptiness assertion below —
/// a refused key must leave no physical item.
#[tokio::test]
async fn transcription_secret_hygiene_refuses_before_any_vault_write() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let vault: Arc<dyn haider_accounts::Vault> = Arc::new(haider_accounts::MemoryVault::default());
    hub.install_accounts(transcription_facade(Arc::clone(&vault)))
        .expect("install accounts");
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
        .expect("control connection");
    let refused = [
        ("t1-empty", "   ".to_owned(), false),
        ("t1-oversized", "x".repeat(513), false),
        ("t1-control", "dg-bad\u{7}sentinel-8d7c".to_owned(), false),
        (
            "t1-clear-with-secret",
            "dg-clear-sentinel-8d7c".to_owned(),
            true,
        ),
    ];
    for (request_id, secret, clear) in refused {
        let response = transcription_request(
            &connection,
            &sink,
            request_id,
            haider_rpc::RequestBody::TranscriptionSecretSet {
                secret: haider_rpc::SecretWire::new(secret),
                clear,
            },
        )
        .await;
        match response {
            haider_rpc::ResponseBody::Error { code, message, .. } => {
                assert_eq!(
                    code,
                    haider_rpc::ERROR_CODE_INVALID_ARGUMENT,
                    "{request_id}"
                );
                assert!(
                    !message.contains("sentinel-8d7c"),
                    "refusals must never echo key material: {message}"
                );
            }
            other => panic!("{request_id}: expected invalid_argument, got {other:?}"),
        }
    }
    assert!(
        vault.list().expect("vault list").is_empty(),
        "a refused key must leave no physical vault item"
    );

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}
