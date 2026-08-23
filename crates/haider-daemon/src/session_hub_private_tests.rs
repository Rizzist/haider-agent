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
use haider_protocol::session_fork::{
    SessionMetaforkProposal, SessionMetaforkRemoval, SessionMetaforkReviewManifest,
};
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
const CANCELLABLE_SHELL_COMMAND: &str =
    "[Console]::Out.Write('started');[Console]::Out.Flush();while($true){Start-Sleep -Seconds 1}";
#[cfg(windows)]
const CANCELLABLE_SHELL_REQUEST_JSON: &str = r#"{"command":"powershell-wait"}"#;

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

#[test]
fn cache_diagnostic_key_is_persistent_exact_length_and_private() {
    let root = tempfile::tempdir().expect("temporary profile");
    let first = load_or_create_cache_diagnostic_key(root.path()).expect("create key");
    let first_bytes = std::fs::read(root.path().join(CACHE_DIAGNOSTIC_KEY_FILE)).expect("read key");
    let second = load_or_create_cache_diagnostic_key(root.path()).expect("reload key");
    let second_bytes =
        std::fs::read(root.path().join(CACHE_DIAGNOSTIC_KEY_FILE)).expect("reread key");
    assert_eq!(first_bytes.len(), 32);
    assert_eq!(
        first_bytes, second_bytes,
        "profile fingerprints survive restart"
    );
    assert_eq!(format!("{first:?}"), "CacheDiagnosticKey([REDACTED])");
    assert_eq!(format!("{second:?}"), "CacheDiagnosticKey([REDACTED])");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = std::fs::metadata(root.path().join(CACHE_DIAGNOSTIC_KEY_FILE))
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "diagnostic key must be owner-only");
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
                    sealed_replay: false,
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

/// MUTATION CHECK: remove `stop_discarded_candidate_actor` from the
/// idempotent-replay mismatch arm. Expected runtime failure: both candidate
/// child IDs remain resident even though only one was committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_duplicate_fork_discards_the_losing_candidate_actor() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let source = SessionId::new("duplicate-fork-source");
    let generation = store.worker_generation();
    hub.create_internal_session(create_command(&source, "duplicate-fork-source"))
        .await
        .expect("source creates");
    let run_id = RunId::new("duplicate-fork-run");
    hub.accept_internal_turn(accept_command(
        &source,
        &run_id,
        generation,
        "duplicate-fork-turn",
    ))
    .await
    .expect("source turn accepts");
    let mut terminal = [run_state_envelope(
        &source,
        &run_id,
        generation,
        "duplicate-fork-done",
        RunState::Done,
    )];
    hub.append(&mut terminal)
        .await
        .expect("source turn completes");
    let source_events = store.read(&source, 0, 64).await.expect("source reads");
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, event.seq))
        })
        .expect("source has fork node");
    let request_json = r#"{"source":"duplicate-fork-source"}"#.to_owned();
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    let command = |candidate: &str| SessionForkCommand {
        command_id: "duplicate-fork-command".into(),
        request_digest: request_digest.clone(),
        request_json: request_json.clone(),
        source_session_id: source.clone(),
        session_id: SessionId::new(candidate),
        worker_generation: generation,
        source_branch_id: None,
        fork_node_id: fork_node_id.clone(),
        fork_seq,
        name: None,
        metafork: None,
        audit_event_id: EventId::new(format!("audit-{candidate}")),
        device_id: DeviceId::new("duplicate-fork-device"),
    };
    let candidate_a = SessionId::new("duplicate-fork-candidate-a");
    let candidate_b = SessionId::new("duplicate-fork-candidate-b");
    let (first, second) = tokio::join!(
        hub.fork_session(command(candidate_a.as_str())),
        hub.fork_session(command(candidate_b.as_str())),
    );
    let created_id = match first.expect("first duplicate returns") {
        SessionForkOutcome::Committed { created, .. }
        | SessionForkOutcome::IdempotentReplay { created } => created.session_id,
    };
    let second_id = match second.expect("second duplicate returns") {
        SessionForkOutcome::Committed { created, .. }
        | SessionForkOutcome::IdempotentReplay { created } => created.session_id,
    };
    assert_eq!(second_id, created_id, "duplicates return one durable child");
    let resident_candidates = {
        let actors = hub.inner.actors.lock().expect("actor registry");
        [&candidate_a, &candidate_b]
            .into_iter()
            .filter(|candidate| actors.contains_key(*candidate))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        resident_candidates,
        vec![created_id],
        "only the committed child candidate remains resident"
    );
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

#[derive(Default)]
struct CapturingFrameSink(Mutex<Vec<WireFrame>>);

impl FrameSink for CapturingFrameSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("capturing sink").push(frame);
        Ok(())
    }
}

/// Whole-subsystem absence must not masquerade as a measured zero/empty
/// snapshot. These are the three legacy sentinel paths the additive
/// `SnapshotAvailabilityWire` field disambiguates.
///
/// MUTATION CHECK: change the missing-account response availability from
/// `Unavailable` to `Available`. Expected runtime failure: the first exact
/// availability assertion below reports the wrong state even though all
/// legacy empty fields remain unchanged.
#[tokio::test]
async fn missing_snapshot_subsystems_publish_reasoned_unavailability() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection opens");
    sink.0.lock().expect("frames").clear();

    for (request_id, body) in [
        (
            "availability-accounts",
            haider_rpc::RequestBody::AccountList { provider: None },
        ),
        (
            "availability-providers",
            haider_rpc::RequestBody::ProviderList { provider: None },
        ),
        ("availability-usage", haider_rpc::RequestBody::UsageReport),
    ] {
        connection
            .request(haider_rpc::RequestId::new(request_id), body)
            .await
            .expect("snapshot request routes");
    }

    {
        let frames = sink.0.lock().expect("frames");
        let availability = |request_id: &str| {
            frames.iter().find_map(|frame| match frame {
                WireFrame::Response {
                    request_id: seen,
                    body:
                        haider_rpc::ResponseBody::AccountList { availability, .. }
                        | haider_rpc::ResponseBody::ProviderList { availability, .. }
                        | haider_rpc::ResponseBody::UsageReport { availability, .. },
                } if seen.as_str() == request_id => availability.as_ref(),
                _ => None,
            })
        };
        assert_eq!(
            availability("availability-accounts"),
            Some(&haider_rpc::SnapshotAvailabilityWire::Unavailable {
                reason: "account subsystem is not configured".into(),
            })
        );
        assert_eq!(
            availability("availability-providers"),
            Some(&haider_rpc::SnapshotAvailabilityWire::Unavailable {
                reason: "provider subsystem is not configured".into(),
            })
        );
        assert_eq!(
            availability("availability-usage"),
            Some(&haider_rpc::SnapshotAvailabilityWire::Unavailable {
                reason: "usage subsystem is not configured".into(),
            })
        );
    }

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Regression pin: a command-response capture borrows the live caller's
/// identity. Dropping that short-lived capture must not unregister the real
/// connection's resident binding or either volatile surface it owns.
#[tokio::test]
async fn command_capture_preserves_callers_binding_and_surface_ownership() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session = SessionId::new("command-capture-owner");
    hub.create_internal_session(create_command(&session, "command-capture-owner"))
        .await
        .expect("session created");
    let generation = store.worker_generation();
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::Control,
                haider_rpc::Capability::View,
            ]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection opens");
    let head = store.latest_seq(&session).await.expect("session head");
    connection
        .request(
            haider_rpc::RequestId::new("command-owner-attach"),
            haider_rpc::RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: head,
                mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attachment opens");
    connection
        .resident_session_binding(Some(session.clone()), generation, None)
        .await
        .expect("resident binding publishes");
    connection
        .request(
            haider_rpc::RequestId::new("command-owner-surface"),
            haider_rpc::RequestBody::SessionSurfacePublish {
                session_id: session.clone(),
                input: Some(haider_rpc::SurfaceInputPublishWire {
                    text: "draft survives".into(),
                    attachments: Vec::new(),
                    revision: 1,
                }),
                status: Some(haider_rpc::SurfaceStatusPublishWire {
                    line: "working survives".into(),
                    state: Some("working".into()),
                    detail: None,
                    revision: 1,
                }),
            },
        )
        .await
        .expect("surface publishes");
    sink.0.lock().expect("frames").clear();

    connection
        .request(
            haider_rpc::RequestId::new("command-owner-rename"),
            haider_rpc::RequestBody::CommandInvoke {
                command_id: haider_rpc::CommandId::new("command-owner-rename-id"),
                command: "/rename Capture survived".into(),
                session_id: Some(session.clone()),
            },
        )
        .await
        .expect("command succeeds");
    let frames = sink.0.lock().expect("frames").clone();
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            WireFrame::Response {
                request_id,
                body: haider_rpc::ResponseBody::CommandInvoke {
                    outcome: haider_rpc::CommandInvokeOutcomeWire::Receipt { receipt },
                },
            } if request_id.as_str() == "command-owner-rename"
                && matches!(receipt.as_ref(), haider_rpc::ResponseBody::SessionRename { .. })
        )),
        "command door must return the successful rename receipt, got {frames:?}"
    );

    assert_eq!(
        hub.inner
            .resident_binding
            .lock()
            .expect("resident binding")
            .visible(),
        Some((Some(session.clone()), generation, None)),
        "the real connection must remain resident-bound after capture drops"
    );
    // Scoped so the guard cannot outlive the block: clippy denies a
    // MutexGuard held across an await, and CI runs with -D warnings.
    {
        let surfaces = hub.inner.surfaces.lock().expect("surfaces");
        let surface = surfaces.get(&session).expect("published surface survives");
        assert_eq!(
            surface.input.as_ref().map(|input| input.owner.as_str()),
            Some(connection.connection_id.as_str())
        );
        assert_eq!(
            surface.status.as_ref().map(|status| status.owner.as_str()),
            Some(connection.connection_id.as_str())
        );
    }

    let connection_id = connection.connection_id.clone();
    drop(connection);
    assert!(
        !hub.inner
            .diagnostic_sinks
            .lock()
            .expect("diagnostic sinks")
            .contains_key(&connection_id),
        "raw drop removes the connection sink"
    );
    assert!(
        !hub.inner
            .resident_binding_viewers
            .lock()
            .expect("binding viewers")
            .contains(&connection_id),
        "raw drop removes the binding viewer"
    );
    assert!(
        hub.inner
            .attachments
            .lock()
            .expect("attachments")
            .values()
            .all(|owner| owner.connection_id != connection_id),
        "raw drop removes every owned attachment"
    );
    {
        let slots = hub.inner.attachment_slots.lock().expect("attachment slots");
        assert_eq!(slots.total, 0, "raw drop refunds the global slot");
        assert!(
            !slots.per_connection.contains_key(&connection_id),
            "raw drop refunds the connection slot"
        );
    }
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// A healthy, empty provider inventory is a known non-retryable absence;
/// poisoned account infrastructure is only an unknown, retryable lookup.
/// The latter must never be collapsed into the former's concrete claim.
#[tokio::test]
async fn provider_command_distinguishes_known_absence_from_lookup_failure() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session = SessionId::new("provider-command-lookup");
    hub.create_internal_session(create_command(&session, "provider-command-lookup"))
        .await
        .expect("session created");
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(
            0,
            Vec::new(),
            vec![provider_summary("known-empty")],
        ),
        vault_supported: false,
        discovery_disabled: false,
        vault: None,
    })
    .expect("accounts install");
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::Control,
                haider_rpc::Capability::View,
            ]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection opens");
    let head = store.latest_seq(&session).await.expect("session head");
    connection
        .request(
            haider_rpc::RequestId::new("provider-command-attach"),
            haider_rpc::RequestBody::SessionAttach {
                session_id: session.clone(),
                after_seq: head,
                mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attachment opens");
    sink.0.lock().expect("frames").clear();

    connection
        .request(
            haider_rpc::RequestId::new("provider-known-empty"),
            haider_rpc::RequestBody::CommandInvoke {
                command_id: haider_rpc::CommandId::new("provider-known-empty-id"),
                command: "/provider known-empty".into(),
                session_id: Some(session.clone()),
            },
        )
        .await
        .expect("known-empty provider routes");
    assert!(
        sink.0.lock().expect("frames").iter().any(|frame| matches!(
            frame,
            WireFrame::Response {
                request_id,
                body: haider_rpc::ResponseBody::Error {
                    code,
                    message,
                    retryable: false,
                    ..
                },
            } if request_id.as_str() == "provider-known-empty"
                && code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT
                && message == "provider known-empty has no daemon-known model"
        )),
        "healthy empty inventory must remain a known absence"
    );
    sink.0.lock().expect("frames").clear();

    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = hub.inner.accounts.lock().expect("accounts lock");
        panic!("poison account facade for lookup-failure pin");
    }));
    assert!(poisoned.is_err(), "poisoning probe must panic");
    connection
        .request(
            haider_rpc::RequestId::new("provider-lookup-unknown"),
            haider_rpc::RequestBody::CommandInvoke {
                command_id: haider_rpc::CommandId::new("provider-lookup-unknown-id"),
                command: "/provider indeterminate".into(),
                session_id: Some(session.clone()),
            },
        )
        .await
        .expect("failed lookup still routes an honest response");
    let frames = sink.0.lock().expect("frames").clone();
    assert!(
        frames.iter().any(|frame| matches!(
            frame,
            WireFrame::Response {
                request_id,
                body: haider_rpc::ResponseBody::Error {
                    code,
                    message,
                    retryable: true,
                    ..
                },
            } if request_id.as_str() == "provider-lookup-unknown"
                && code == "provider_models_unknown"
                && message.contains("could not determine")
        )),
        "infrastructure failure must be retryable unknown state, got {frames:?}"
    );

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Metafork review is exact authorization, so a removal must be wholly
/// contained in copied history. Accepting an intersecting range would make
/// the reviewed request claim sequence coordinates the child never copied.
#[tokio::test]
async fn metafork_rejects_range_beyond_copied_lineage() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let source = SessionId::new("metafork-overrun-parent");
    hub.create_internal_session(create_command(&source, "metafork-overrun-parent"))
        .await
        .expect("create source");
    let run_id = RunId::new("metafork-overrun-run");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "metafork-overrun-turn".into(),
        request_digest: "metafork-overrun-turn-digest".into(),
        request_json: r#"{"text":"bounded review event"}"#.into(),
        session_id: source.clone(),
        worker_generation: store.worker_generation(),
        branch_id: None,
        run_id: run_id.clone(),
        agent_id: None,
        text: "bounded review event".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new("metafork-overrun-queued"),
        user_event_id: EventId::new("metafork-overrun-user"),
        active_event_id: EventId::new("metafork-overrun-active"),
        device_id: DeviceId::new("metafork-overrun-device"),
    })
    .await
    .expect("accept source turn");
    let mut done = [run_state_envelope(
        &source,
        &run_id,
        store.worker_generation(),
        "metafork-overrun-done",
        RunState::Done,
    )];
    hub.append(&mut done)
        .await
        .expect("terminalize source turn");
    let source_events = store.read(&source, 0, 64).await.expect("source events");
    let user_seq = source_events
        .iter()
        .find(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::UserMessage { ref text, .. }) if text == "bounded review event"
            )
        })
        .expect("source user event")
        .seq;
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&run_id)).then_some((node.node, event.seq))
        })
        .expect("source fork node");
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::Control]),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("connection opens");
    let proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: user_seq,
            through_seq: fork_seq + 1,
            reason: "range intentionally overruns copied history".into(),
            preview: None,
            reviewed_events: Vec::new(),
        }],
    };

    let error = connection
        .canonical_metafork_proposal(
            &source,
            store.worker_generation(),
            None,
            &fork_node_id,
            fork_seq,
            &proposal,
        )
        .await
        .expect_err("intersecting but uncontained range must be rejected");
    assert!(matches!(
        error,
        SessionHubError::Store(ref error)
            if error.code == haider_protocol::error::ErrorCode::InvalidArgument
                && error.message.contains("contained")
    ));

    connection.close().await.expect("connection closes");
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// MUTATION CHECK: route a proposal-only `session.metafork` through
/// `fork_session` or claim its command receipt. Expected RUNTIME failure: the
/// session roster gains a child, the source journal moves, or a receipt exists
/// before the operator echoes the reviewed proposal digest.
///
/// MUTATION CHECK: remove the connection-local reviewed-manifest lookup.
/// Expected RUNTIME failure: the direct accepted request commits without the
/// operator ever receiving that command's review response.
///
/// MUTATION CHECK: digest only `model_proposal` instead of the full review
/// manifest. Expected RUNTIME failure: the altered-description acceptance
/// commits a child journaling an instruction the operator never reviewed.
///
/// MUTATION CHECK: change the review response to `review_manifest: None`.
/// Expected RUNTIME failure: the operator cannot inspect the complete source,
/// fork coordinate, child name, directive, and exact event roster being gated.
#[tokio::test]
async fn metafork_review_is_write_free_until_human_acceptance() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let source = SessionId::new("metafork-review-parent");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-metafork-review-parent".into(),
        request_digest: "create-metafork-review-parent-digest".into(),
        request_json: r#"{"session":"metafork-review-parent"}"#.into(),
        session_id: source.clone(),
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
        event_id: EventId::new("created-metafork-review-parent"),
        device_id: DeviceId::new("metafork-review-device"),
    })
    .await
    .expect("create source");
    let run_id = RunId::new("metafork-review-run");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "accept-metafork-review-turn".into(),
        request_digest: "accept-metafork-review-turn-digest".into(),
        request_json: r#"{"text":"review this chocolate event"}"#.into(),
        session_id: source.clone(),
        worker_generation: store.worker_generation(),
        branch_id: None,
        run_id: run_id.clone(),
        agent_id: None,
        text: "review this chocolate event".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
        queued_event_id: EventId::new("metafork-review-queued"),
        user_event_id: EventId::new("metafork-review-user"),
        active_event_id: EventId::new("metafork-review-active"),
        device_id: DeviceId::new("metafork-review-device"),
    })
    .await
    .expect("accept review source turn");
    let mut done = [run_state_envelope(
        &source,
        &run_id,
        store.worker_generation(),
        "metafork-review-done",
        RunState::Done,
    )];
    hub.append(&mut done)
        .await
        .expect("terminalize review source turn");
    let source_events = store.read(&source, 0, 64).await.expect("source events");
    let user_seq = source_events
        .iter()
        .find(|event| {
            matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::UserMessage { ref text, .. })
                    if text == "review this chocolate event"
            )
        })
        .expect("review source user event")
        .seq;
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&run_id)).then_some((node.node, event.seq))
        })
        .expect("review source fork node");
    let source_before = store.read(&source, 0, 64).await.expect("source before");
    let sessions_before = store.session_ids().await.expect("sessions before");
    let command_id = haider_rpc::CommandId::new("metafork-review-command");
    let proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: user_seq,
            through_seq: user_seq,
            reason: "model proposal awaiting operator review".into(),
            preview: Some("source sequence 1".into()),
            reviewed_events: Vec::new(),
        }],
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
            haider_rpc::RequestId::new("metafork-review-attach"),
            haider_rpc::RequestBody::SessionAttach {
                session_id: source.clone(),
                after_seq: 0,
                mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach source control");
    sink.0.lock().expect("frames").clear();
    connection
        .request(
            haider_rpc::RequestId::new("metafork-review-request"),
            haider_rpc::RequestBody::SessionMetafork {
                command_id: command_id.clone(),
                session_id: source.clone(),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: fork_node_id.clone(),
                fork_seq,
                name: Some("reviewed child".into()),
                description: "remove the proposed source event".into(),
                model_proposal: proposal,
                accepted_proposal_digest: None,
            },
        )
        .await
        .expect("review response");
    let (reviewed, review_manifest, reviewed_digest) = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                body:
                    haider_rpc::ResponseBody::SessionMetafork {
                        committed: false,
                        session_id: None,
                        model_proposal,
                        review_manifest: Some(review_manifest),
                        proposal_digest,
                        ..
                    },
                ..
            } => Some((
                model_proposal.clone(),
                review_manifest.clone(),
                proposal_digest.clone(),
            )),
            _ => None,
        })
        .expect("write-free metafork review response");
    assert_eq!(review_manifest.command_id, command_id.0);
    assert_eq!(review_manifest.source_session_id, source);
    assert_eq!(review_manifest.fork_node_id, fork_node_id);
    assert_eq!(review_manifest.fork_seq, fork_seq);
    assert_eq!(review_manifest.name.as_deref(), Some("reviewed child"));
    assert_eq!(
        review_manifest.description,
        "remove the proposed source event"
    );
    assert_eq!(review_manifest.model_proposal, reviewed);
    assert_eq!(
        review_manifest.digest().expect("review manifest digest"),
        reviewed_digest
    );
    let preview = reviewed.removals[0]
        .preview
        .as_deref()
        .expect("source-derived removal preview");
    assert_eq!(preview, "1 prompt-visible event(s); see reviewed_events");
    assert!(!preview.contains("source sequence 1"));
    assert_eq!(reviewed.removals[0].reviewed_events.len(), 1);
    let reviewed_event = &reviewed.removals[0].reviewed_events[0];
    assert_eq!(reviewed_event.source_seq, user_seq);
    assert_eq!(reviewed_event.payload_kind, "user_message");
    assert!(
        reviewed_event
            .excerpt
            .contains("review this chocolate event")
    );
    let unreviewed_command = haider_rpc::CommandId::new("metafork-direct-accept-command");
    let unreviewed_digest = SessionMetaforkReviewManifest {
        command_id: unreviewed_command.0.clone(),
        source_session_id: source.clone(),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id: fork_node_id.clone(),
        fork_seq,
        name: Some("reviewed child".into()),
        description: "remove the proposed source event".into(),
        model_proposal: reviewed.clone(),
    }
    .digest()
    .expect("unreviewed manifest digest");
    connection
        .request(
            haider_rpc::RequestId::new("metafork-direct-accept-request"),
            haider_rpc::RequestBody::SessionMetafork {
                command_id: unreviewed_command.clone(),
                session_id: source.clone(),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: fork_node_id.clone(),
                fork_seq,
                name: Some("reviewed child".into()),
                description: "remove the proposed source event".into(),
                model_proposal: reviewed.clone(),
                accepted_proposal_digest: Some(unreviewed_digest),
            },
        )
        .await
        .expect("direct acceptance is answered");
    assert!(sink.0.lock().expect("frames").iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::Error { .. },
        } if request_id.as_str() == "metafork-direct-accept-request"
    )));
    assert_eq!(
        store.read(&source, 0, 64).await.expect("source after"),
        source_before
    );
    assert_eq!(
        store.session_ids().await.expect("sessions after"),
        sessions_before
    );
    assert!(
        store
            .session_metafork_receipt(
                command_id.0.clone(),
                "unused-review-digest".into(),
                "{}".into(),
            )
            .await
            .expect("receipt lookup")
            .is_none()
    );

    connection
        .request(
            haider_rpc::RequestId::new("metafork-altered-description"),
            haider_rpc::RequestBody::SessionMetafork {
                command_id: command_id.clone(),
                session_id: source.clone(),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: fork_node_id.clone(),
                fork_seq,
                name: Some("reviewed child".into()),
                description: "an instruction that was not reviewed".into(),
                model_proposal: reviewed.clone(),
                accepted_proposal_digest: Some(reviewed_digest.clone()),
            },
        )
        .await
        .expect("altered acceptance is answered");
    assert!(sink.0.lock().expect("frames").iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::Error { .. },
        } if request_id.as_str() == "metafork-altered-description"
    )));
    assert_eq!(
        store
            .session_ids()
            .await
            .expect("sessions after altered accept"),
        sessions_before
    );

    let accepted_request = haider_rpc::RequestBody::SessionMetafork {
        command_id: command_id.clone(),
        session_id: source.clone(),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id,
        fork_seq,
        name: Some("reviewed child".into()),
        description: "remove the proposed source event".into(),
        model_proposal: reviewed,
        accepted_proposal_digest: Some(reviewed_digest),
    };
    connection
        .request(
            haider_rpc::RequestId::new("metafork-reviewed-accept"),
            accepted_request.clone(),
        )
        .await
        .expect("reviewed acceptance commits");
    assert_eq!(
        store
            .read(&source, 0, 64)
            .await
            .expect("source after accept"),
        source_before
    );
    assert_eq!(
        store
            .session_ids()
            .await
            .expect("sessions after accept")
            .len(),
        sessions_before.len() + 1
    );
    let first_commit = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    body @ haider_rpc::ResponseBody::SessionMetafork {
                        committed: true, ..
                    },
            } if request_id.as_str() == "metafork-reviewed-accept" => Some(body.clone()),
            _ => None,
        })
        .expect("committed metafork response");
    connection
        .request(
            haider_rpc::RequestId::new("metafork-reviewed-replay"),
            accepted_request,
        )
        .await
        .expect("accepted receipt replay");
    let replay_commit = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    body @ haider_rpc::ResponseBody::SessionMetafork {
                        committed: true, ..
                    },
            } if request_id.as_str() == "metafork-reviewed-replay" => Some(body.clone()),
            _ => None,
        })
        .expect("replayed metafork response");
    assert_eq!(replay_commit, first_commit);
    assert!(
        store
            .session_metafork_receipt(
                unreviewed_command.0,
                "unused-direct-digest".into(),
                "{}".into(),
            )
            .await
            .expect("direct receipt lookup")
            .is_none()
    );

    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

#[derive(Default)]
struct RefusingRequiredFrameSink {
    accepted_baseline: AtomicBool,
    closed: AtomicBool,
}

impl FrameSink for RefusingRequiredFrameSink {
    fn try_send(&self, _frame: WireFrame) -> Result<(), FrameSendError> {
        if self.accepted_baseline.swap(true, Ordering::AcqRel) {
            Err(FrameSendError)
        } else {
            Ok(())
        }
    }

    fn close_after_required_delivery_failure(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// MUTATION CHECK: restore the one-line `visible()`-only baseline admission.
/// Expected runtime failure: this never-bound viewer captures zero frames
/// instead of one explicit unbound frame in the current daemon generation.
#[tokio::test]
async fn resident_session_binding_never_bound_viewer_receives_unbound_baseline() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let generation = store.worker_generation();
    let sink = Arc::new(CapturingFrameSink::default());

    let viewer = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("never-bound viewer opens");

    assert_eq!(
        sink.0.lock().expect("viewer frames").as_slice(),
        &[WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: generation,
            binding_token: None,
        }]
    );
    viewer.close().await.expect("viewer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: replace the one-line `visible()` baseline selection with
/// the synthesized unbound tuple. Expected runtime failure: the late viewer
/// receives `None` instead of the resident publisher's bound session.
#[tokio::test]
async fn resident_session_binding_bound_viewer_still_receives_binding_baseline() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session = SessionId::new("resident-binding-bound-baseline");
    hub.create_internal_session(create_command(&session, "resident-binding-bound-baseline"))
        .await
        .expect("session created");
    let generation = store.worker_generation();
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let publisher = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("publisher opens");
    publisher
        .resident_session_binding(Some(session.clone()), generation, None)
        .await
        .expect("publisher binds");
    let sink = Arc::new(CapturingFrameSink::default());

    let viewer = hub
        .open_connection(
            capabilities,
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("bound viewer opens");

    assert_eq!(
        sink.0.lock().expect("viewer frames").as_slice(),
        &[WireFrame::ResidentSessionBinding {
            session_id: Some(session),
            worker_generation: generation,
            binding_token: None,
        }]
    );
    publisher.close().await.expect("publisher closes");
    viewer.close().await.expect("viewer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: change the one-line `if may_view_binding` baseline guard
/// to `if false`. Expected runtime failure: the authorized viewer becomes
/// indistinguishable from the capability-empty connection because both
/// capture zero frames.
#[tokio::test]
async fn resident_session_binding_unbound_baseline_is_distinct_from_silence() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let generation = store.worker_generation();
    let viewer_sink = Arc::new(CapturingFrameSink::default());
    let silent_sink = Arc::new(CapturingFrameSink::default());

    let viewer = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            viewer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("viewer opens");
    let capability_empty = hub
        .open_connection(
            std::collections::BTreeSet::new(),
            silent_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("capability-empty connection opens");

    assert_eq!(
        viewer_sink.0.lock().expect("viewer frames").as_slice(),
        &[WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: generation,
            binding_token: None,
        }],
        "authorized absence is an explicit frame"
    );
    assert!(
        silent_sink.0.lock().expect("silent frames").is_empty(),
        "the capability-empty connection demonstrates actual no-frame silence"
    );
    viewer.close().await.expect("viewer closes");
    capability_empty
        .close()
        .await
        .expect("capability-empty connection closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: replace the retained baseline read in `open_connection`
/// with `None`. Expected runtime failure: after the pressured observer is
/// closed, its replacement receives no current binding and remains stale.
#[tokio::test]
async fn resident_session_binding_recovers_refusal_with_late_subscriber_baseline() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session = SessionId::new("resident-binding-baseline");
    hub.create_internal_session(create_command(&session, "resident-binding-baseline"))
        .await
        .expect("session created");
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let publisher = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("publisher connection");
    let refusing_sink = Arc::new(RefusingRequiredFrameSink::default());
    let refused_observer = hub
        .open_connection(
            capabilities.clone(),
            refusing_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer opens before a baseline exists");
    let generation = store.worker_generation();

    publisher
        .resident_session_binding(Some(session.clone()), generation, None)
        .await
        .expect("binding is retained despite observer refusal");
    assert!(
        refusing_sink.closed.load(Ordering::Acquire),
        "required-state refusal closes the stale transport"
    );
    refused_observer
        .close()
        .await
        .expect("refused observer closes");

    let replacement_sink = Arc::new(CapturingFrameSink::default());
    let replacement = hub
        .open_connection(
            capabilities,
            replacement_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("replacement observer receives retained baseline");
    assert_eq!(
        replacement_sink
            .0
            .lock()
            .expect("replacement frames")
            .as_slice(),
        &[WireFrame::ResidentSessionBinding {
            session_id: Some(session),
            worker_generation: generation,
            binding_token: None,
        }]
    );

    publisher.close().await.expect("publisher closes");
    replacement.close().await.expect("replacement closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: replace the stored `binding_token` in
/// `publish_resident_binding` with `None`. Expected runtime failure: both the
/// same-connection hop and the different-connection publication lose their
/// client-minted correlators, so the observer cannot distinguish the two.
#[tokio::test]
async fn resident_binding_distinguishes_a_hop_from_a_second_surface() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let first_session = SessionId::new("binding-token-first");
    let hopped_session = SessionId::new("binding-token-hop");
    hub.create_internal_session(create_command(&first_session, "binding-token-first"))
        .await
        .expect("first session created");
    hub.create_internal_session(create_command(&hopped_session, "binding-token-hop"))
        .await
        .expect("hop session created");
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let first_surface_sink = Arc::new(CapturingFrameSink::default());
    let first_surface = hub
        .open_connection(
            capabilities.clone(),
            first_surface_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("first surface opens");
    let observer_sink = Arc::new(CapturingFrameSink::default());
    let observer = hub
        .open_connection(
            capabilities.clone(),
            observer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer opens");
    let generation = store.worker_generation();

    first_surface
        .resident_session_binding(
            Some(first_session.clone()),
            generation,
            Some("not a sane token".into()),
        )
        .await
        .expect("invalid token is rejected without closing the publisher");
    assert!(
        first_surface_sink
            .0
            .lock()
            .expect("publisher frames")
            .iter()
            .any(|frame| matches!(
                frame,
                WireFrame::ProtocolError(error)
                    if error.code == haider_rpc::ERROR_CODE_INVALID_ARGUMENT && !error.fatal
            ))
    );
    assert_eq!(
        observer_sink.0.lock().expect("observer frames").len(),
        1,
        "invalid token is never stored or echoed"
    );

    first_surface
        .resident_session_binding(
            Some(first_session.clone()),
            generation,
            Some("surface-A".into()),
        )
        .await
        .expect("first surface binds");
    first_surface
        .resident_session_binding(
            Some(hopped_session.clone()),
            generation,
            Some("surface-A".into()),
        )
        .await
        .expect("same connection hops with the same token");

    let second_surface_sink = Arc::new(CapturingFrameSink::default());
    let second_surface = hub
        .open_connection(
            capabilities,
            second_surface_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("second surface opens");
    assert_eq!(
        second_surface_sink
            .0
            .lock()
            .expect("second surface baseline")
            .as_slice(),
        &[WireFrame::ResidentSessionBinding {
            session_id: Some(hopped_session.clone()),
            worker_generation: generation,
            binding_token: Some("surface-A".into()),
        }],
        "a late viewer receives the current publisher's correlator"
    );
    second_surface
        .resident_session_binding(
            Some(hopped_session.clone()),
            generation,
            Some("surface-B".into()),
        )
        .await
        .expect("different connection publishes its token");

    assert_eq!(
        observer_sink.0.lock().expect("observer frames").as_slice(),
        &[
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(first_session),
                worker_generation: generation,
                binding_token: Some("surface-A".into()),
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(hopped_session.clone()),
                worker_generation: generation,
                binding_token: Some("surface-A".into()),
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(hopped_session),
                worker_generation: generation,
                binding_token: Some("surface-B".into()),
            },
        ],
        "same token + new session is a hop; different token is a second surface"
    );

    second_surface.close().await.expect("second surface closes");
    first_surface.close().await.expect("first surface closes");
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: delete the one-line owner comparison in
/// `clear_resident_binding`. Expected runtime failure: closing the overlapped
/// old publisher emits a same-generation unbind that clears the replacement.
#[tokio::test]
async fn resident_session_binding_old_owner_cannot_clear_replacement() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session = SessionId::new("resident-binding-owner");
    hub.create_internal_session(create_command(&session, "resident-binding-owner"))
        .await
        .expect("session created");
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let first = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("first publisher");
    let observer_sink = Arc::new(CapturingFrameSink::default());
    let observer = hub
        .open_connection(
            capabilities.clone(),
            observer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer");
    let generation = store.worker_generation();
    first
        .resident_session_binding(Some(session.clone()), generation, None)
        .await
        .expect("first bind");

    let replacement = hub
        .open_connection(
            capabilities,
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("replacement publisher");
    replacement
        .resident_session_binding(Some(session.clone()), generation, None)
        .await
        .expect("same-state reannounce transfers ownership");
    first.close().await.expect("old publisher closes");
    assert_eq!(
        observer_sink.0.lock().expect("observer frames").as_slice(),
        &[
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(session),
                worker_generation: generation,
                binding_token: None,
            },
        ],
        "old-owner close cannot append an unbind"
    );

    replacement.close().await.expect("current publisher closes");
    assert!(matches!(
        observer_sink.0.lock().expect("observer frames").last(),
        Some(WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: observed_generation,
            binding_token: None,
        }) if *observed_generation == generation
    ));
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: replace the registry's `max_by_key` selection with the
/// closing publisher. Expected runtime failure: closing the newest resident
/// emits `None` instead of restoring the still-live predecessor's session.
#[tokio::test]
async fn resident_session_binding_restores_live_predecessor() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let first_session = SessionId::new("resident-binding-predecessor");
    let second_session = SessionId::new("resident-binding-newest");
    hub.create_internal_session(create_command(
        &first_session,
        "resident-binding-predecessor",
    ))
    .await
    .expect("predecessor session created");
    hub.create_internal_session(create_command(&second_session, "resident-binding-newest"))
        .await
        .expect("newest session created");
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let first = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("first publisher");
    let second = hub
        .open_connection(
            capabilities.clone(),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("second publisher");
    let observer_sink = Arc::new(CapturingFrameSink::default());
    let observer = hub
        .open_connection(
            capabilities,
            observer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer");
    let generation = store.worker_generation();

    first
        .resident_session_binding(Some(first_session.clone()), generation, None)
        .await
        .expect("predecessor binds");
    second
        .resident_session_binding(Some(second_session.clone()), generation, None)
        .await
        .expect("newest binds");
    second.close().await.expect("newest publisher closes");

    assert_eq!(
        observer_sink.0.lock().expect("observer frames").as_slice(),
        &[
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(first_session.clone()),
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(second_session),
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(first_session),
                worker_generation: generation,
                binding_token: None,
            },
        ]
    );

    first.close().await.expect("predecessor closes");
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: replace the one-line `may_view_binding` capability
/// expression with `true`. Expected runtime failure: a capability-empty
/// client receives the retained session id and explicit unbind transition.
#[tokio::test]
async fn resident_session_binding_requires_view_for_baseline_and_push() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session = SessionId::new("resident-binding-view-gate");
    hub.create_internal_session(create_command(&session, "resident-binding-view-gate"))
        .await
        .expect("session created");
    let publisher = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::Control]),
            Arc::new(CapturingFrameSink::default()),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("publisher");
    let generation = store.worker_generation();
    publisher
        .resident_session_binding(Some(session), generation, None)
        .await
        .expect("bind publishes");

    let denied_sink = Arc::new(CapturingFrameSink::default());
    let denied = hub
        .open_connection(
            std::collections::BTreeSet::new(),
            denied_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("capability-empty connection remains valid");
    publisher
        .resident_session_binding(None, generation, None)
        .await
        .expect("unbind publishes");
    assert!(
        denied_sink.0.lock().expect("denied frames").is_empty(),
        "session identity is not visible without View or Control"
    );

    publisher.close().await.expect("publisher closes");
    denied.close().await.expect("denied connection closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: delete the one-line `publish_resident_binding` call in
/// `HubConnection::resident_session_binding`. Expected runtime failure: the
/// observer misses the bind, rebind, and explicit unbind transition frames.
#[tokio::test]
async fn resident_session_binding_fans_out_bind_rebind_and_unbind() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let first = SessionId::new("resident-binding-first");
    let second = SessionId::new("resident-binding-second");
    hub.create_internal_session(create_command(&first, "resident-binding-first"))
        .await
        .expect("first session created");
    hub.create_internal_session(create_command(&second, "resident-binding-second"))
        .await
        .expect("second session created");

    let publisher_sink = Arc::new(CapturingFrameSink::default());
    let observer_sink = Arc::new(CapturingFrameSink::default());
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let publisher = hub
        .open_connection(
            capabilities.clone(),
            publisher_sink,
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("publisher connection");
    let observer = hub
        .open_connection(
            capabilities,
            observer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer connection");
    let generation = store.worker_generation();

    publisher
        .resident_session_binding(Some(first.clone()), generation, None)
        .await
        .expect("bind publishes");
    publisher
        .resident_session_binding(Some(second.clone()), generation, None)
        .await
        .expect("rebind publishes");
    publisher
        .resident_session_binding(None, generation, None)
        .await
        .expect("unbind publishes");

    assert_eq!(
        observer_sink.0.lock().expect("observer frames").as_slice(),
        &[
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(first),
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(second),
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
        ]
    );

    publisher.close().await.expect("publisher closes");
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub stops");
}

/// MUTATION CHECK: change the generation comparison in
/// `resident_session_binding` from `!=` to `==`. Expected runtime failure: a
/// stale post-rebind unbind reaches the observer and clears its fresh state.
#[tokio::test]
async fn resident_session_binding_discards_stale_generation_after_rebind() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let current = SessionId::new("resident-binding-current");
    hub.create_internal_session(create_command(&current, "resident-binding-current"))
        .await
        .expect("current session created");

    let publisher_sink = Arc::new(CapturingFrameSink::default());
    let observer_sink = Arc::new(CapturingFrameSink::default());
    let capabilities = std::collections::BTreeSet::from([
        haider_rpc::Capability::Control,
        haider_rpc::Capability::View,
    ]);
    let publisher = hub
        .open_connection(
            capabilities.clone(),
            publisher_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("publisher connection");
    let observer = hub
        .open_connection(
            capabilities,
            observer_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer connection");
    let generation = store.worker_generation();
    assert!(generation > 0, "worker generations are positive fences");

    publisher
        .resident_session_binding(Some(current.clone()), generation, None)
        .await
        .expect("fresh rebind publishes");
    publisher
        .resident_session_binding(None, generation - 1, None)
        .await
        .expect("stale publish is rejected without closing the connection");

    assert_eq!(
        observer_sink.0.lock().expect("observer frames").as_slice(),
        &[
            WireFrame::ResidentSessionBinding {
                session_id: None,
                worker_generation: generation,
                binding_token: None,
            },
            WireFrame::ResidentSessionBinding {
                session_id: Some(current),
                worker_generation: generation,
                binding_token: None,
            },
        ]
    );
    assert!(
        publisher_sink
            .0
            .lock()
            .expect("publisher frames")
            .iter()
            .any(|frame| matches!(
                frame,
                WireFrame::ProtocolError(error)
                    if error.code == haider_rpc::ERROR_CODE_STALE_GENERATION && !error.fatal
            )),
        "publisher receives the standard non-fatal stale-generation rejection"
    );

    publisher.close().await.expect("publisher closes");
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub stops");
}

/// The pipe location is daemon-owned: the view RPC returns the exact absolute
/// resolver output for a durable session and applies the standard session
/// not-found response before exposing any synthesized filename.
#[tokio::test]
async fn session_pipe_path_resolves_absolute_path_and_rejects_unknown_session() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("pipe-path-rpc-session");
    hub.create_internal_session(create_command(&session_id, "pipe-path-rpc"))
        .await
        .expect("session created");

    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("view connection");
    sink.0.lock().expect("frames").clear();
    connection
        .request(
            haider_rpc::RequestId::new("pipe-path-happy"),
            haider_rpc::RequestBody::SessionPipePath {
                session_id: session_id.clone(),
            },
        )
        .await
        .expect("pipe path response");
    connection
        .request(
            haider_rpc::RequestId::new("pipe-path-missing"),
            haider_rpc::RequestBody::SessionPipePath {
                session_id: SessionId::new("missing-pipe-path-session"),
            },
        )
        .await
        .expect("missing response");

    let expected_path = root
        .path()
        .join("pipe")
        .join("pipe-path-rpc-session.pipe")
        .to_string_lossy()
        .into_owned();
    {
        let frames = sink.0.lock().expect("frames");
        assert!(matches!(
            &frames[0],
            WireFrame::Response {
                request_id,
                body: haider_rpc::ResponseBody::SessionPipePath { path },
            } if request_id.as_str() == "pipe-path-happy"
                && path == &expected_path
                && std::path::Path::new(path).is_absolute()
        ));
        assert!(matches!(
            &frames[1],
            WireFrame::Response {
                request_id,
                body: haider_rpc::ResponseBody::Error { code, message, .. },
            } if request_id.as_str() == "pipe-path-missing"
                && code == haider_rpc::ERROR_CODE_NOT_FOUND
                && message == "session was not found"
        ));
    }

    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
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
    view_sink.0.lock().expect("view frames").clear();
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
    control_sink.0.lock().expect("control frames").clear();
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
            branch_id: None,
            agent_id: None,
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
            None,
            None,
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
            branch_id: None,
            agent_id: None,
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
            None,
            None,
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

/// An accepted direct command can outlive the in-memory handoff that would
/// normally start it. Manager drain must recognize its durable origin marker,
/// close the command as prompt-visible Cancelled, and only then settle Idle.
#[tokio::test]
async fn accepted_shell_without_handoff_drains_with_prompt_visible_completion() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("direct-shell-unhanded-drain");
    let run_id = RunId::new("direct-shell-unhanded-drain-run");
    let item_id = ItemId::new("direct-shell-unhanded-drain-item");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "direct-shell-unhanded-drain"))
        .await
        .expect("typed session commits");
    hub.accept_shell_exec(ShellExecAcceptCommand {
        command_id: "direct-shell-unhanded-drain-command".into(),
        request_digest: "direct-shell-unhanded-drain-digest".into(),
        request_json: r#"{"command":"printf never-started"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        branch_id: None,
        agent_id: None,
        run_id: run_id.clone(),
        item_id: item_id.clone(),
        command: "printf never-started".into(),
        running_event_id: EventId::new("direct-shell-unhanded-drain-running"),
        item_event_id: EventId::new("direct-shell-unhanded-drain-started"),
        active_event_id: EventId::new("direct-shell-unhanded-drain-active"),
        device_id: DeviceId::new("worker-law-test"),
    })
    .await
    .expect("shell acceptance commits");

    crate::worker::WorkerManager::start(
        hub.clone(),
        crate::worker::WorkerDependencies::unconfigured_for_tests(),
        false,
    )
    .shutdown()
    .await
    .expect("manager drains accepted work");

    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let completed = history
        .iter()
        .find(|envelope| {
            envelope.run_id.as_ref() == Some(&run_id)
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::Item(ItemEvent::Completed {
                                item_id: ref candidate,
                                item: TurnItem::CommandExecution {
                                    status: ToolStatus::Cancelled,
                                    ..
                                },
                            }) if candidate == &item_id
                        )
                    },
                )
        })
        .expect("drain closes the command");
    assert_eq!(completed.render.prompt, PromptRender::Verbatim);
    assert!(completed.branch_id.is_none() && completed.agent_id.is_none());
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Supervisor failure recovery shares the same durable-origin discriminator:
/// marked direct commands expose their Failed completion, while existing
/// unmarked model-turn recovery remains prompt-omitted.
#[tokio::test]
async fn shell_supervisor_exit_keeps_failed_completion_prompt_visible() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("direct-shell-panic-exit");
    let run_id = RunId::new("direct-shell-panic-exit-run");
    let item_id = ItemId::new("direct-shell-panic-exit-item");
    let generation = store.worker_generation();
    hub.create_session(create_command(&session_id, "direct-shell-panic-exit"))
        .await
        .expect("typed session commits");
    hub.accept_shell_exec(ShellExecAcceptCommand {
        command_id: "direct-shell-panic-exit-command".into(),
        request_digest: "direct-shell-panic-exit-digest".into(),
        request_json: r#"{"command":"printf never-started"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        branch_id: None,
        agent_id: None,
        run_id: run_id.clone(),
        item_id: item_id.clone(),
        command: "printf never-started".into(),
        running_event_id: EventId::new("direct-shell-panic-exit-running"),
        item_event_id: EventId::new("direct-shell-panic-exit-started"),
        active_event_id: EventId::new("direct-shell-panic-exit-active"),
        device_id: DeviceId::new("worker-law-test"),
    })
    .await
    .expect("shell acceptance commits");

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, 1)
        .await
        .expect("supervisor exit terminalizes shell");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let completed = history
        .iter()
        .find(|envelope| {
            envelope.run_id.as_ref() == Some(&run_id)
                && serde_json::from_value::<EventPayload>(envelope.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::Item(ItemEvent::Completed {
                                item_id: ref candidate,
                                item: TurnItem::CommandExecution {
                                    status: ToolStatus::Failed,
                                    ..
                                },
                            }) if candidate == &item_id
                        )
                    },
                )
        })
        .expect("failure closes the command");
    assert_eq!(completed.render.prompt, PromptRender::Verbatim);
    assert!(completed.branch_id.is_none() && completed.agent_id.is_none());
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
    frames.retain(|frame| !matches!(frame, WireFrame::ResidentSessionBinding { .. }));
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

/// effect_recovery_v1: a run parked in the crash-window state maps to the
/// typed `EffectUnknown` wire state (not swallowed into Running/Unknown),
/// and it out-ranks other states in the observed-run selector so the rail
/// surfaces the parked crash window over concurrent activity.
///
/// MUTATION CHECK: map EffectOutcomeUnknown to Running/Unknown, or drop its
/// top priority in select_observed_run. Expected runtime failure: the wire
/// state is wrong, or a parked crash window hides behind another run.
#[test]
fn effect_outcome_unknown_maps_to_typed_state_and_outranks() {
    use crate::session_hub::rpc::observe_run_state;
    use haider_protocol::state::RunState;
    use haider_rpc::ObserveRunStateWire;

    assert!(matches!(
        observe_run_state(&RunState::EffectOutcomeUnknown),
        ObserveRunStateWire::EffectUnknown
    ));
    // The neighbours stay distinct (no accidental collapse).
    assert!(matches!(
        observe_run_state(&RunState::Errored),
        ObserveRunStateWire::Errored
    ));
    assert!(matches!(
        observe_run_state(&RunState::Cancelled),
        ObserveRunStateWire::Cancelled
    ));
    assert!(matches!(
        observe_run_state(&RunState::Thinking),
        ObserveRunStateWire::Running
    ));
}
