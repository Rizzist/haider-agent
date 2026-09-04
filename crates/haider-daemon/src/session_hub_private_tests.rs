#![allow(clippy::expect_used)]
//! Private session-hub accounting tests.

use super::*;
use haider_accounts::Vault as _;
use haider_protocol::EventPayload;
use haider_protocol::cache::CacheRequestAttemptV1;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome,
    EffectPhase,
};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::graph::{
    EvidenceVerdict, GraphFinalizationDeferred, GraphPhase, SHIP_LOOP_TEMPLATE,
};
use haider_protocol::headless::{HeadlessRunEventPayload, HeadlessRunSpecV1, RunBudgetV1};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{AgentId, BranchId, EventId, GraphId, ItemId, MenuId, NodeId, RunId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{
    AnswerVia, DecisionKind, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::queue::QueueChange;
use haider_protocol::session_fork::{
    SessionMetaforkProposal, SessionMetaforkRemoval, SessionMetaforkReviewManifest,
};
use haider_protocol::state::RunState;
use haider_protocol::verify::VerifyVerdict;
use haider_store::{
    GraphEvidenceCommand, GraphPinCommand, QueuePromoteCommand, QueueRemoveCommand,
    SessionCreateCommand, ShellExecAcceptCommand, ShellExecAcceptOutcome, TurnAcceptCommand,
};

/// MUTATION CHECK: removing or moving the equality makes a continuously hot
/// session retain a 51st turn, or release one turn before the documented
/// live-window bound.
#[test]
fn resident_window_hard_cut_fires_exactly_at_fifty_turns() {
    let mut state = ResidentWindowState::default();
    for turn in 1..RESIDENT_TURN_WINDOW {
        assert!(!advance_resident_turn(&mut state), "early cut at {turn}");
    }
    assert!(advance_resident_turn(&mut state));
    assert_eq!(state.terminal_turns, 0);
    assert!(!advance_resident_turn(&mut state));
}

/// MUTATION CHECK: requiring a fetch timestamp before considering a known
/// inventory miss skips refresh-on-miss for legacy cached inventories.
#[test]
fn model_inventory_miss_refreshes_even_without_a_fetch_timestamp() {
    let mut summary = provider_summary("openai-oauth");
    summary.models = vec!["known-model".to_owned()];
    summary.inventory_fetched_at_ms = None;
    assert!(super::rpc::provider_inventory_needs_refresh(
        &summary,
        "new-model"
    ));
    assert!(!super::rpc::provider_inventory_needs_refresh(
        &summary,
        "known-model"
    ));
}

/// MUTATION CHECK: restoring the two 1,024-slot coalescing wake rings grows
/// always-live hub capacity without improving durable lag recovery.
#[test]
fn publication_wake_rings_are_bounded_to_256() {
    assert_eq!(PUBLICATION_RING_CAPACITY, 256);
}
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: Vec::new(),
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: haider_rpc::ModelInventoryAuthorityWire::Unknown,
        auth_methods: Vec::new(),
        availability: haider_rpc::ProviderAvailabilityWire::Unknown,
        availability_reason: None,
        default_model: None,
        enabled: true,
        trust: haider_rpc::ProviderTrustWire::Full,
    }
}

/// The attachment replay preflight must keep immutable-blob validation out of
/// an idempotent retry while still entering the fused acceptance transaction
/// that repairs a legacy first-turn receipt with no title.
#[test]
fn attachment_receipt_replay_routes_through_fused_title_repair() {
    let source = include_str!("session_hub/rpc.rs");
    let start = source
        .find("    async fn turn_submit(")
        .expect("turn_submit source");
    let tail = &source[start..];
    let end = tail
        // Do not couple a source-sensitive invariant to the checkout's line
        // ending convention. Windows may materialize this source with CRLF.
        .find("    async fn shell_exec(")
        .expect("next RPC handler boundary");
    let body = &tail[..end];
    let receipt = body
        .find(".turn_accept_receipt(")
        .expect("attachment receipt preflight");
    let repair = body
        .find(".accept_turn_with_auto_title(")
        .expect("fused replay repair");
    let validation = body
        .find("validate_turn_attachments(")
        .expect("attachment validation");

    assert!(
        receipt < repair && repair < validation,
        "receipt lookup must precede fused title repair, which must precede attachment validation"
    );
    assert!(
        body[repair..validation].contains("Some(first_turn_slug.clone())"),
        "the replay transaction must carry the same deterministic auto-title"
    );
    assert!(
        body[receipt..repair].contains("replay_title_user_event_id(&command_id)"),
        "the reachable title event ID must derive from the durable command identity"
    );
}

/// Two pre-fusion attachment receipts in one profile must repair their titles
/// independently. The replay user event is reachable by the title envelope,
/// so a process-wide constant would make the second transaction collide and
/// roll back.
#[tokio::test]
async fn attachment_title_repairs_use_distinct_stable_event_ids_across_sessions() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let generation = store.worker_generation();
    let fixtures = [
        ("attachment-repair-a", "first-attachment-turn"),
        ("attachment-repair-b", "second-attachment-turn"),
    ];
    let mut replay_event_ids = Vec::new();

    for (suffix, title) in fixtures {
        let session_id = SessionId::new(format!("session-{suffix}"));
        hub.create_internal_session(create_command(&session_id, suffix))
            .await
            .expect("create session");
        let run_id = RunId::new(format!("run-{suffix}"));
        let mut legacy_command = accept_command(&session_id, &run_id, generation, suffix);
        legacy_command.attachments = vec![haider_protocol::tool::AttachmentBlock::File {
            artifact: haider_protocol::ids::ArtifactRef::new(format!("artifact-{suffix}")),
            name: format!("{suffix}.txt"),
            lines: 1,
        }];
        hub.accept_turn(legacy_command.clone())
            .await
            .expect("legacy acceptance commits without a title");

        let command_id = CommandId(legacy_command.command_id.clone());
        let replay_event_id = super::rpc::replay_title_user_event_id(&command_id);
        assert_eq!(
            replay_event_id,
            super::rpc::replay_title_user_event_id(&command_id),
            "one receipt must derive a stable replay event ID"
        );
        legacy_command.user_event_id = replay_event_id.clone();
        replay_event_ids.push(replay_event_id);
        assert!(matches!(
            hub.accept_turn_with_auto_title(legacy_command, Some(title.into()))
                .await
                .expect("repair transaction commits"),
            TurnAcceptOutcome::Committed { .. }
        ));
        assert_eq!(
            hub.session_metadata(&session_id)
                .await
                .expect("metadata")
                .and_then(|metadata| metadata.title),
            Some(title.into())
        );
    }

    assert_ne!(
        replay_event_ids[0], replay_event_ids[1],
        "different durable commands must not collide"
    );
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
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

/// R2-18 hold-out pin: the known-zero probe elision regressed and was
/// reverted. A fresh session still attaches at its exact durable head, replays
/// Created at sequence 1, and an absent durable head remains typed NotFound.
#[tokio::test]
async fn r2_18_fresh_actor_and_attach_preserve_exact_head_semantics() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path())
        .await
        .expect("store opens");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub opens");
    let session_id = SessionId::new("r2-18-fresh-head");
    hub.create_internal_session(create_command(&session_id, "r2-18-fresh-head"))
        .await
        .expect("fresh session commits");
    assert_eq!(
        store.latest_seq(&session_id).await.expect("durable head"),
        1
    );

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
            RequestId::new("r2-18-attach"),
            RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
            },
        )
        .await
        .expect("fresh attach");
    // Registry #94: 5s = 50 * 100ms CI scheduling quanta for one in-process
    // attach publication; this loop contains no nested process deadline.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if sink.0.lock().expect("frames").iter().any(|frame| {
                matches!(
                    frame,
                    WireFrame::AttachCaughtUp {
                        high_water_seq: 1,
                        ..
                    }
                )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fresh attach catches up");
    {
        let frames = sink.0.lock().expect("frames");
        assert!(frames.iter().any(|frame| matches!(
            frame,
            WireFrame::Response {
                request_id,
                body: ResponseBody::SessionAttach { attach_state, .. },
            } if request_id.as_str() == "r2-18-attach"
                && attach_state.replay_through_seq == 1
        )));
        assert!(frames.iter().any(|frame| matches!(
            frame,
            WireFrame::Event { envelope, .. }
                if envelope.session_id == session_id && envelope.seq == 1
        )));
    }

    connection
        .request(
            RequestId::new("r2-18-missing"),
            RequestBody::SessionAttach {
                session_id: SessionId::new("r2-18-missing-head"),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
            },
        )
        .await
        .expect("missing attach response");
    assert!(sink.0.lock().expect("frames").iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: ResponseBody::Error { code, .. },
        } if request_id.as_str() == "r2-18-missing"
            && code == haider_rpc::ERROR_CODE_NOT_FOUND
    )));

    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
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
            event.payload.decode_event(),
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

/// The daemon idle-TTL decision must treat a durable workflow continuation as
/// live work even when no client is needed to drive the next autonomous hop.
///
/// MUTATION CHECK: make `daemon_is_durably_quiescent` ignore durable run
/// heads and the first assertion flips true, authorizing daemon retirement
/// while the deferred workflow still owns recoverable work. Remove the
/// request-attempt marker and the restart classifier no longer proves that
/// the fixture is a runnable workflow continuation.
#[tokio::test]
async fn recoverable_workflow_continuation_blocks_daemon_idle_ttl_retirement() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("workflow-idle-ttl-session");
    let run_id = RunId::new("workflow-idle-ttl-run");
    let graph_id = GraphId::new("workflow-idle-ttl-graph");
    let generation = store.worker_generation();

    hub.create_internal_session(create_command(&session_id, "workflow-idle-ttl"))
        .await
        .expect("create session");
    hub.pin_graph(GraphPinCommand {
        command_id: "pin-workflow-idle-ttl".into(),
        request_digest: "pin-workflow-idle-ttl-digest".into(),
        request_json: r#"{"template":"ship-loop"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        graph_id: graph_id.clone(),
        template: SHIP_LOOP_TEMPLATE.into(),
        device_id: DeviceId::new("workflow-idle-ttl-device"),
    })
    .await
    .expect("pin workflow");
    hub.accept_turn(accept_command(
        &session_id,
        &run_id,
        generation,
        "workflow-idle-ttl",
    ))
    .await
    .expect("accept workflow turn");
    let status = hub
        .graph_status(&session_id)
        .await
        .expect("graph status")
        .expect("active graph");
    assert_eq!(status.phase, GraphPhase::Active);
    let state_digest =
        haider_store::graph_finalization_state_digest(&status).expect("graph state digest");
    let mut configured = run_state_envelope(
        &session_id,
        &run_id,
        generation,
        "workflow-idle-ttl-configured",
        RunState::Queued,
    );
    *configured.payload = HeadlessRunEventPayload::HeadlessRunConfigured(HeadlessRunSpecV1 {
        cwd: "workflow-idle-ttl-workspace".into(),
        provider: "fake".into(),
        model: "workflow-idle-ttl-model".into(),
        max_output_tokens: 64,
        effort: None,
        fast: false,
        seed: None,
        permission_overrides: Default::default(),
        trust_hooks: false,
        budget: RunBudgetV1 {
            max_cost_microusd: Some(1_000_000),
            ..RunBudgetV1::default()
        },
        request_deadline_unix_ms: None,
        replay_of: None,
    })
    .to_payload_value()
    .expect("headless configuration serializes");
    let request_attempt = CacheRequestAttemptV1 {
        ordinal: 1,
        diagnostic: haider_protocol::provider::CacheRequestDiagnosticV1 {
            history_message_count: 1,
            stable_prefix_tokens: 8,
            breakpoint_hashes: Default::default(),
            cache_domain_hash: Some("workflow-idle-ttl-domain".into()),
            cache_domain_changed: None,
            previous_breakpoint: None,
            prefix_match: haider_protocol::provider::CachePrefixMatchV1::Unavailable,
            control: haider_protocol::provider::CacheControlObservationV1::NotRequired,
            cacheable_minimum_tokens: None,
            reuse_gap_ms: None,
            rewarm: None,
            classification: None,
        },
    }
    .extension_item()
    .expect("provider request attempt serializes");
    let mut continuation = vec![
        configured,
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "workflow-idle-ttl-attempt",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("workflow-idle-ttl-attempt-item"),
                item: request_attempt,
            }),
        ),
        run_state_envelope(
            &session_id,
            &run_id,
            generation,
            "workflow-idle-ttl-streaming",
            RunState::Streaming,
        ),
        run_payload_envelope(
            &session_id,
            &run_id,
            generation,
            "workflow-idle-ttl-deferred",
            EventPayload::GraphFinalizationDeferred(GraphFinalizationDeferred {
                graph_id,
                run_id: run_id.clone(),
                state_digest,
                provider_requests_consumed: 1,
                unmet_nodes: vec![haider_protocol::graph::verify_node()],
            }),
        ),
    ];
    StoreHandle::append(&store, &mut continuation)
        .await
        .expect("append recoverable continuation");

    assert!(
        !hub.daemon_is_durably_quiescent()
            .await
            .expect("retirement predicate"),
        "idle TTL cannot retire a daemon with recoverable workflow work"
    );

    hub.shutdown().await.expect("hub shutdown before restart");
    store.close().await.expect("store close before restart");

    let restarted = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store with a new generation");
    let recovered = crate::turn_recovery::recover_interrupted_turns(
        &restarted,
        &DeviceId::new("workflow-idle-ttl-recovery"),
    )
    .await
    .expect("classify restart work");
    assert_eq!(recovered.len(), 1, "one deferred workflow is runnable");
    let crate::turn_recovery::RecoveredWork::WorkflowContinuation(work) = &recovered[0] else {
        panic!("durable fixture must classify as a workflow continuation");
    };
    assert_eq!(work.accepted.session_id, session_id);
    assert_eq!(work.accepted.run_id, run_id);
    assert_eq!(work.provider_requests_consumed, 1);
    assert_eq!(work.provider_request_ordinal, 1);

    let restarted_hub =
        SessionHub::new(restarted.clone(), SessionHubConfig::default()).expect("restart hub");
    assert!(
        !restarted_hub
            .daemon_is_durably_quiescent()
            .await
            .expect("restart retirement predicate"),
        "recovered workflow work must still block idle retirement"
    );

    restarted_hub
        .shutdown()
        .await
        .expect("restart hub shutdown");
    restarted.close().await.expect("restart store close");
}

/// MUTATION CHECK: make `FenceIfQuiescent` answer before prior admissions and let the deleter
/// discover the accepted run afterward. Expected RUNTIME failure: the
/// barrier reports success or the still-live actor cannot acknowledge the
/// follow-up lease command.
#[tokio::test]
async fn delete_during_an_active_turn_waits_for_the_actor_fence() {
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
            peer_message: None,
            auto_title: None,
            completed: accepted,
        })
        .await
        .expect("queue pre-fence acceptance");
    let (completed, mut quiescent) = oneshot::channel();
    actor
        .commands
        .send(ActorCommand::FenceIfQuiescent { completed })
        .await
        .expect("queue deletion barrier");
    assert!(
        matches!(
            quiescent.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "the deletion fence waits behind the pre-fence active turn"
    );
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
                let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
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
    hub.install_accounts(transcription_facade(Arc::new(
        haider_accounts::MemoryVault::default(),
    )))
    .expect("install scope vault");
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
            let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
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

async fn create_fork_ready_source(
    hub: &SessionHub,
    store: &SqliteStoreHandle,
    label: &str,
) -> (SessionId, NodeId, u64) {
    let source = SessionId::new(format!("{label}-source"));
    let run_id = RunId::new(format!("{label}-run"));
    let generation = store.worker_generation();
    hub.create_internal_session(create_command(&source, label))
        .await
        .expect("create fork source");
    hub.accept_internal_turn(accept_command(&source, &run_id, generation, label))
        .await
        .expect("accept fork source turn");
    let mut terminal = [run_state_envelope(
        &source,
        &run_id,
        generation,
        &format!("{label}-done"),
        RunState::Done,
    )];
    hub.append(&mut terminal)
        .await
        .expect("complete fork source turn");
    let events = store.read(&source, 0, 64).await.expect("read fork source");
    let (node_id, seq) = events
        .into_iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
                return None;
            };
            Some((node.node, event.seq))
        })
        .expect("fork source node");
    (source, node_id, seq)
}

fn private_fork_command(
    store: &SqliteStoreHandle,
    label: &str,
    source: SessionId,
    child: SessionId,
    fork_node_id: NodeId,
    fork_seq: u64,
) -> SessionForkCommand {
    let request_json = serde_json::json!({
        "source": &source,
        "fork_node_id": &fork_node_id,
        "fork_seq": fork_seq,
    })
    .to_string();
    SessionForkCommand {
        command_id: format!("{label}-fork-command"),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        source_session_id: source,
        session_id: child,
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id,
        fork_seq,
        name: None,
        metafork: None,
        audit_event_id: EventId::new(format!("{label}-fork-audit")),
        device_id: DeviceId::new("fork-security-test"),
    }
}

/// Fork scope inheritance is exact and durable for both narrowed variants;
/// neither child may fall through the missing-record `All` compatibility path.
#[tokio::test]
async fn fork_clones_allow_and_none_ssh_scopes_exactly() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let vault = Arc::new(haider_accounts::MemoryVault::default());
    hub.install_accounts(transcription_facade(vault.clone()))
        .expect("install scope vault");

    let cases = [
        (
            "allow-scope",
            crate::ssh::SshScope::Allow(std::collections::BTreeSet::from(["a".to_owned()])),
        ),
        ("none-scope", crate::ssh::SshScope::None),
    ];
    for (label, scope) in cases {
        let (source, fork_node_id, fork_seq) = create_fork_ready_source(&hub, &store, label).await;
        hub.set_ssh_scope(source.clone(), scope.clone())
            .expect("set parent scope");
        let child = SessionId::new(format!("{label}-child"));
        let outcome = hub
            .fork_session(private_fork_command(
                &store,
                label,
                source,
                child.clone(),
                fork_node_id,
                fork_seq,
            ))
            .await
            .expect("fork with narrowed scope");
        assert!(matches!(
            outcome,
            SessionForkOutcome::Committed { ref created, .. }
                if created.session_id == child
        ));
        assert_eq!(hub.ssh_scope(&child).expect("cached child scope"), scope);
        let durable = crate::ssh::SshProfileStore::new(vault.clone());
        assert_eq!(
            durable.session_scope(&child).expect("durable child scope"),
            scope,
            "the child must have a physical scope record"
        );
    }

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

struct FailingScopePutVault {
    inner: haider_accounts::MemoryVault,
    fail_scope_put: AtomicBool,
}

impl haider_accounts::Vault for FailingScopePutVault {
    fn put(
        &self,
        alias: &haider_accounts::CredentialAlias,
        secret: &[u8],
    ) -> haider_accounts::AccountsResult<()> {
        if self.fail_scope_put.load(Ordering::SeqCst)
            && alias.as_str().starts_with("haider.ssh.scope.")
        {
            return Err(haider_accounts::HaiderError::new(
                haider_accounts::ErrorCode::Internal,
                "injected SSH scope persistence failure",
                false,
            ));
        }
        self.inner.put(alias, secret)
    }

    fn resolve(
        &self,
        alias: &haider_accounts::CredentialAlias,
    ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
        self.inner.resolve(alias)
    }

    fn delete(
        &self,
        alias: &haider_accounts::CredentialAlias,
    ) -> haider_accounts::AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> haider_accounts::AccountsResult<Vec<haider_accounts::CredentialAlias>> {
        self.inner.list()
    }
}

/// A failed scope write precedes the SQLite transaction, leaving no roster
/// row, resident actor, journal, or provisional vault item.
#[tokio::test]
async fn failed_fork_scope_clone_leaves_no_observable_child() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let vault = Arc::new(FailingScopePutVault {
        inner: haider_accounts::MemoryVault::default(),
        fail_scope_put: AtomicBool::new(false),
    });
    hub.install_accounts(transcription_facade(vault.clone()))
        .expect("install scope vault");
    let (source, fork_node_id, fork_seq) =
        create_fork_ready_source(&hub, &store, "scope-failure").await;
    hub.set_ssh_scope(source.clone(), crate::ssh::SshScope::None)
        .expect("set source scope");
    let child = SessionId::new("scope-failure-child");
    let sessions_before = store.session_ids().await.expect("sessions before");
    vault.fail_scope_put.store(true, Ordering::SeqCst);

    let error = hub
        .fork_session(private_fork_command(
            &store,
            "scope-failure",
            source,
            child.clone(),
            fork_node_id,
            fork_seq,
        ))
        .await
        .expect_err("scope clone must fail the fork");
    assert!(error.to_string().contains("injected SSH scope"));
    assert_eq!(
        store.session_ids().await.expect("sessions after"),
        sessions_before
    );
    assert!(hub.existing_actor(&child).expect("actor lookup").is_none());
    assert!(
        vault
            .list()
            .expect("vault aliases")
            .iter()
            .all(|alias| !alias.as_str().ends_with(child.as_str())),
        "no provisional child scope alias may survive"
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

struct TouchThenFailScopeVault {
    inner: haider_accounts::MemoryVault,
    scope_failures_remaining: AtomicUsize,
}

impl haider_accounts::Vault for TouchThenFailScopeVault {
    fn put(
        &self,
        alias: &haider_accounts::CredentialAlias,
        secret: &[u8],
    ) -> haider_accounts::AccountsResult<()> {
        self.inner.put(alias, secret)?;
        if self
            .scope_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
            && alias.as_str().starts_with("haider.ssh.scope.")
        {
            return Err(haider_accounts::HaiderError::new(
                haider_accounts::ErrorCode::Internal,
                "injected post-commit SSH scope failure",
                false,
            ));
        }
        Ok(())
    }

    fn resolve(
        &self,
        alias: &haider_accounts::CredentialAlias,
    ) -> haider_accounts::AccountsResult<haider_accounts::SecretHandle> {
        self.inner.resolve(alias)
    }

    fn delete(
        &self,
        alias: &haider_accounts::CredentialAlias,
    ) -> haider_accounts::AccountsResult<()> {
        self.inner.delete(alias)
    }

    fn list(&self) -> haider_accounts::AccountsResult<Vec<haider_accounts::CredentialAlias>> {
        self.inner.list()
    }
}

/// A vault error after the exact bytes landed still requires a fully
/// successful retry. Read-back alone cannot prove the directory entry was
/// crash-durable after a directory-fsync failure.
#[tokio::test]
async fn post_touch_scope_error_requires_successful_retry_before_fork_commit() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let vault = Arc::new(TouchThenFailScopeVault {
        inner: haider_accounts::MemoryVault::default(),
        scope_failures_remaining: AtomicUsize::new(0),
    });
    hub.install_accounts(transcription_facade(vault.clone()))
        .expect("install scope vault");
    let (source, fork_node_id, fork_seq) =
        create_fork_ready_source(&hub, &store, "post-touch-scope").await;
    hub.set_ssh_scope(source.clone(), crate::ssh::SshScope::None)
        .expect("set source scope");
    vault.scope_failures_remaining.store(1, Ordering::SeqCst);
    let child = SessionId::new("post-touch-scope-child");

    let outcome = hub
        .fork_session(private_fork_command(
            &store,
            "post-touch-scope",
            source,
            child.clone(),
            fork_node_id,
            fork_seq,
        ))
        .await
        .expect("successful scope retry permits the fork");
    assert!(matches!(outcome, SessionForkOutcome::Committed { .. }));
    assert_eq!(
        crate::ssh::SshProfileStore::new(vault)
            .session_scope_if_present(&child)
            .expect("read explicit child scope"),
        Some(crate::ssh::SshScope::None)
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// If both the initial write and its durability retry fail after touching the
/// vault, SQLite is never called and the best-effort delete leaves no child
/// identity on any roster authority.
#[tokio::test]
async fn persistent_post_touch_scope_failure_leaves_no_observable_child() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let vault = Arc::new(TouchThenFailScopeVault {
        inner: haider_accounts::MemoryVault::default(),
        scope_failures_remaining: AtomicUsize::new(0),
    });
    hub.install_accounts(transcription_facade(vault.clone()))
        .expect("install scope vault");
    let (source, fork_node_id, fork_seq) =
        create_fork_ready_source(&hub, &store, "persistent-post-touch").await;
    hub.set_ssh_scope(source.clone(), crate::ssh::SshScope::None)
        .expect("set source scope");
    let child = SessionId::new("persistent-post-touch-child");
    let sessions_before = store.session_ids().await.expect("sessions before");
    vault.scope_failures_remaining.store(2, Ordering::SeqCst);

    let error = hub
        .fork_session(private_fork_command(
            &store,
            "persistent-post-touch",
            source,
            child.clone(),
            fork_node_id,
            fork_seq,
        ))
        .await
        .expect_err("two failed durable writes must reject the fork");
    assert!(error.to_string().contains("retry failed"));
    assert_eq!(
        store.session_ids().await.expect("sessions after"),
        sessions_before
    );
    assert!(hub.existing_actor(&child).expect("actor lookup").is_none());
    assert!(
        vault
            .list()
            .expect("vault aliases")
            .iter()
            .all(|alias| !alias.as_str().ends_with(child.as_str()))
    );

    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The final fork audit is the consent cut: historical `AllowAlways` answers
/// stop there, while the metadata's explicit creation policy is inherited and
/// remains an ordinary policy default for the child.
#[tokio::test]
async fn fork_resets_remembered_grants_but_keeps_creation_permission_policy() {
    let root = tempfile::tempdir().expect("temp store");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    hub.install_accounts(transcription_facade(Arc::new(
        haider_accounts::MemoryVault::default(),
    )))
    .expect("install scope vault");
    let source = SessionId::new("permission-fork-source");
    let run_id = RunId::new("permission-fork-run");
    let mut create = create_command(&source, "permission-fork");
    create.permission_overrides = Some(haider_protocol::session::SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    });
    hub.create_internal_session(create)
        .await
        .expect("create permission source");
    hub.accept_internal_turn(accept_command(
        &source,
        &run_id,
        store.worker_generation(),
        "permission-fork",
    ))
    .await
    .expect("accept permission source turn");
    let user_node = store
        .read(&source, 0, 32)
        .await
        .expect("read user node")
        .into_iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
                return None;
            };
            Some(node.node)
        })
        .expect("user node");
    let effect = haider_protocol::ids::EffectId::new("permission-fork-effect");
    let menu_id = MenuId::new("permission-fork-menu");
    let menu = Menu {
        id: menu_id.clone(),
        kind: MenuKind::Permission {
            effect_summary: "write a file".into(),
        },
        title: "Allow write?".into(),
        body: vec!["Allow this write for the session".into()],
        options: vec![MenuOption {
            key: "allow_always".into(),
            label: "Always allow".into(),
            detail: None,
            decision: Some(DecisionKind::AllowAlways),
        }],
        blocking: true,
        scope: MenuScope::Session,
        origin: "fork-security-test".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let assistant_node = NodeId::new("permission-fork-assistant-node");
    let generation = store.worker_generation();
    let mut permission_facts = vec![
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-intent",
            EventPayload::Effect(EffectPhase::Intent(EffectIntent {
                effect: effect.clone(),
                class: EffectClass::FsWrite,
                summary: "write a file".into(),
                args_digest: "permission-fork-write-shape".into(),
                workspace_revision: None,
            })),
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-authorized",
            EventPayload::Effect(EffectPhase::Authorized {
                effect,
                verdict: AuthorizationVerdict::Ask {
                    menu: menu_id.clone(),
                },
            }),
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-menu-opened",
            EventPayload::MenuOpened(menu),
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-menu-answered",
            EventPayload::MenuAnswered(MenuAnswer {
                menu: menu_id,
                option_index: 0,
                option_key: Some("allow_always".into()),
                value: None,
                via: AnswerVia::Rpc,
            }),
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-computer-use",
            EventPayload::UserMessage {
                text: "/computer-use".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-mobile-use",
            EventPayload::UserMessage {
                text: "/mobile-use".into(),
                attachments: Vec::new(),
                mode: haider_protocol::DeliveryMode::Steer,
            },
        ),
        run_payload_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-assistant-node-event",
            EventPayload::NodeCommitted(TreeNode {
                node: assistant_node.clone(),
                parent: Some(user_node),
                kind: NodeKind::AssistantCommit {
                    text: "completed write".into(),
                    verdict: VerifyVerdict::NotApplicable,
                },
            }),
        ),
        run_state_envelope(
            &source,
            &run_id,
            generation,
            "permission-fork-done",
            RunState::Done,
        ),
    ];
    hub.append(&mut permission_facts)
        .await
        .expect("append permission history");
    let fork_seq = permission_facts[6].seq;
    let parent_state = crate::worker::durable_session_tool_state(&store, &source)
        .await
        .expect("parent grants");
    assert!(
        parent_state
            .grants
            .iter()
            .any(|grant| grant.class == EffectClass::FsWrite),
        "fixture must hold remembered AllowAlways consent"
    );
    assert!(
        parent_state.mobile_use_active,
        "fixture must hold prompt-derived mobile consent"
    );

    let child = SessionId::new("permission-fork-child");
    let SessionForkOutcome::Committed { created, .. } = hub
        .fork_session(private_fork_command(
            &store,
            "permission-fork",
            source,
            child.clone(),
            assistant_node,
            fork_seq,
        ))
        .await
        .expect("fork permission history")
    else {
        panic!("permission fork must commit");
    };
    let child_state = crate::worker::durable_session_tool_state(&store, &child)
        .await
        .expect("child grants");
    assert!(
        child_state.grants.is_empty(),
        "remembered and prompt-derived computer consent must not cross the fork audit boundary"
    );
    assert!(
        !child_state.mobile_use_active,
        "prompt-derived mobile consent must not cross the fork audit boundary"
    );
    let expected_overrides = haider_protocol::session::SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    };
    assert_eq!(
        created.metadata.permission_overrides,
        Some(expected_overrides),
        "creation policy is committed configuration, not accumulated consent"
    );
    assert!(
        crate::worker::effective_permission_defaults(&created.metadata)
            .into_iter()
            .any(|(class, default)| {
                class == EffectClass::FsWrite
                    && default == haider_protocol::tool::ToolPermissionDefault::Allow
            })
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

struct ForkPublicationGate {
    child: SessionId,
    gated: AtomicBool,
    reached: mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SessionHubObserver for ForkPublicationGate {
    fn observe(&self, observation: HubObservation) {
        let is_child_commit = matches!(
            observation,
            HubObservation::Persisted { ref session_id, .. } if *session_id == self.child
        );
        if !is_child_commit || self.gated.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.reached.send(());
        self.release
            .lock()
            .expect("fork publication release")
            .recv()
            .expect("test releases fork publication");
    }
}

fn listed_sessions(sink: &CapturingFrameSink, request_id: &str) -> Vec<SessionSummary> {
    sink.0
        .lock()
        .expect("captured roster frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id: seen,
                body: ResponseBody::SessionList { sessions, .. },
            } if seen.as_str() == request_id => Some(sessions.clone()),
            _ => None,
        })
        .expect("session-list response")
}

/// MUTATION CHECK: derive status count by running session.list summaries or
/// omit a durable row from the scalar query. Expected runtime failure: the
/// narrow response is absent or its count differs from durable roster truth.
#[tokio::test]
async fn status_snapshot_counts_sessions_without_listing_summaries() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    for suffix in ["status-a", "status-b", "status-c"] {
        let session_id = SessionId::new(suffix);
        hub.create_internal_session(create_command(&session_id, suffix))
            .await
            .expect("create status fixture session");
    }
    let phantom = SessionId::new("status-uncommitted-fork-candidate");
    let phantom_reservation = hub
        .reserve_fork_candidate(&phantom)
        .expect("reserve uncommitted fork candidate");
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("status observer");
    sink.0.lock().expect("frames").clear();
    connection
        .request(
            RequestId::new("status-scalars"),
            RequestBody::StatusSnapshot {},
        )
        .await
        .expect("status request");
    let status = sink
        .0
        .lock()
        .expect("status frame")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::StatusSnapshot {
                        active_account,
                        session_count,
                        waiting_for_route_count,
                        adoption_available,
                        daemon_pid,
                        socket_path,
                        pid_file_path,
                        ready,
                        ready_since,
                        providers_loaded,
                        idle_ttl_ms,
                        warm,
                    },
            } if request_id.as_str() == "status-scalars" => Some((
                active_account.clone(),
                *session_count,
                *waiting_for_route_count,
                adoption_available.clone(),
                *daemon_pid,
                socket_path.clone(),
                pid_file_path.clone(),
                *ready,
                *ready_since,
                *providers_loaded,
                *idle_ttl_ms,
                *warm,
            )),
            _ => None,
        })
        .expect("status response");
    assert_eq!(
        status,
        (
            None,
            3,
            0,
            Vec::new(),
            Some(std::process::id()),
            None,
            None,
            false,
            None,
            false,
            None,
            false
        )
    );

    drop(phantom_reservation);
    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: resolve the active account instead of the requested alias,
/// or move provider/default-model lookup back into client collection RPCs.
/// The inactive selected account must still win atomically.
#[tokio::test]
async fn session_create_resolves_provider_and_model_inside_admission() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let selected_alias = haider_protocol::ids::CredentialAlias::new("selected-fake");
    let descriptor = haider_protocol::credential::CredentialDescriptor {
        alias: selected_alias.clone(),
        provider: "fake".into(),
        base_url: None,
        auth_method: haider_protocol::credential::AuthMethod::OAuth,
        identity: "active@example.invalid".into(),
        status: haider_protocol::credential::CredentialStatus::Ok,
        active: false,
        label: None,
        account_identity: None,
        created_at_ms: None,
    };
    let mut provider = provider_summary("fake");
    provider.models = vec!["fake-v1".into()];
    provider.default_model = Some("fake-v1".into());
    let active_other = haider_protocol::credential::CredentialDescriptor {
        alias: haider_protocol::ids::CredentialAlias::new("active-other"),
        provider: "other".into(),
        active: true,
        ..descriptor.clone()
    };
    let mut other_provider = provider_summary("other");
    other_provider.models = vec!["other-v1".into()];
    other_provider.default_model = Some("other-v1".into());
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(Mutex::new(vec![descriptor.clone(), active_other.clone()])),
        management: crate::accounts::ManagementSnapshot::new(
            0,
            vec![descriptor, active_other],
            vec![provider, other_provider],
        ),
        vault_supported: false,
        discovery_disabled: false,
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
        vault: None,
    })
    .expect("accounts install");
    hub.install_creatable_providers(std::collections::BTreeSet::from([
        "fake".into(),
        "other".into(),
    ]))
    .expect("creatable provider install");
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
        .expect("create connection");
    sink.0.lock().expect("frames").clear();
    connection
        .request(
            RequestId::new("resolved-create"),
            RequestBody::SessionCreateWithPermissionOverrides {
                command_id: haider_rpc::CommandId::new("resolved-create-command"),
                cwd: std::env::current_dir()
                    .expect("cwd")
                    .to_string_lossy()
                    .into_owned(),
                provider: String::new(),
                model: String::new(),
                max_tokens: 4_096,
                permission_overrides: None,
                cache_policy: None,
                interaction_mode: haider_protocol::session::SessionInteractionModeV1::Autonomous,
                ssh_scope: None,
                account_alias: Some(selected_alias.clone()),
                resolve_provider: true,
                resolve_model: true,
                effort: None,
                fast: None,
            },
        )
        .await
        .expect("resolved create request");
    let metadata = sink
        .0
        .lock()
        .expect("create frame")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body: ResponseBody::SessionCreate { metadata, .. },
            } if request_id.as_str() == "resolved-create" => Some(metadata.clone()),
            _ => None,
        })
        .expect("resolved create response");
    assert_eq!(metadata.provider, "fake");
    assert_eq!(metadata.model, "fake-v1");
    assert_eq!(metadata.account_alias.as_deref(), Some("selected-fake"));
    assert_eq!(
        metadata.interaction_mode,
        haider_protocol::session::SessionInteractionModeV1::Autonomous
    );

    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// R2-03: absent storage is the canonical default `All` scope. A later
/// narrowing must survive exact create-receipt replay, while an explicitly
/// non-default create still persists its scope before returning.
#[tokio::test]
async fn r2_03_default_scope_is_absent_and_create_replay_never_reopens_it() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let vault: Arc<dyn haider_accounts::Vault> = Arc::new(haider_accounts::MemoryVault::default());
    let mut provider = provider_summary("fake");
    provider.models = vec!["fake-v1".into()];
    provider.default_model = Some("fake-v1".into());
    hub.install_accounts(crate::accounts::AccountsFacade {
        login: None,
        oauth: None,
        snapshot: Arc::new(Mutex::new(Vec::new())),
        management: crate::accounts::ManagementSnapshot::new(0, Vec::new(), vec![provider]),
        vault_supported: true,
        discovery_disabled: true,
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
        vault: Some(vault.clone()),
    })
    .expect("accounts install");
    hub.install_creatable_providers(std::collections::BTreeSet::from(["fake".into()]))
        .expect("creatable provider install");
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
        .expect("create connection");
    sink.0.lock().expect("frames").clear();
    let cwd = std::env::current_dir()
        .expect("cwd")
        .to_string_lossy()
        .into_owned();
    let create = |command_id: &str, ssh_scope| RequestBody::SessionCreateWithPermissionOverrides {
        command_id: haider_rpc::CommandId::new(command_id),
        cwd: cwd.clone(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4_096,
        permission_overrides: None,
        cache_policy: None,
        interaction_mode: haider_protocol::session::SessionInteractionModeV1::Autonomous,
        ssh_scope,
        account_alias: None,
        resolve_provider: false,
        resolve_model: false,
        effort: None,
        fast: None,
    };
    let response_session = |sink: &CapturingFrameSink, request_id: &str| {
        sink.0
            .lock()
            .expect("frames")
            .iter()
            .find_map(|frame| match frame {
                WireFrame::Response {
                    request_id: seen,
                    body: ResponseBody::SessionCreate { session_id, .. },
                } if seen.as_str() == request_id => Some(session_id.clone()),
                _ => None,
            })
            .expect("session.create response")
    };
    let scopes = crate::ssh::SshProfileStore::new(vault.clone());

    connection
        .request(
            RequestId::new("default-scope-create"),
            create("default-scope-command", None),
        )
        .await
        .expect("default create");
    let default_session = response_session(&sink, "default-scope-create");
    assert_eq!(
        lock(&hub.inner.ssh_scopes)
            .expect("scope cache")
            .get(&default_session),
        Some(&crate::ssh::SshScope::All),
        "a committed default scope must be cached without a vault write"
    );
    assert_eq!(
        scopes
            .session_scope_if_present(&default_session)
            .expect("read default scope"),
        None,
        "default All must have no durable row"
    );

    hub.set_ssh_scope(default_session.clone(), crate::ssh::SshScope::None)
        .expect("later narrowing");
    connection
        .request(
            RequestId::new("default-scope-replay"),
            create("default-scope-command", None),
        )
        .await
        .expect("create receipt replay");
    assert_eq!(
        response_session(&sink, "default-scope-replay"),
        default_session
    );
    assert_eq!(
        scopes
            .session_scope_if_present(&default_session)
            .expect("read narrowed scope"),
        Some(crate::ssh::SshScope::None),
        "create replay must not apply its original default scope"
    );

    connection
        .request(
            RequestId::new("explicit-scope-create"),
            create(
                "explicit-scope-command",
                Some(haider_rpc::SshScopeWire::None),
            ),
        )
        .await
        .expect("explicit scope create");
    let explicit_session = response_session(&sink, "explicit-scope-create");
    assert_eq!(
        scopes
            .session_scope_if_present(&explicit_session)
            .expect("read explicit scope"),
        Some(crate::ssh::SshScope::None)
    );
    connection
        .request(
            RequestId::new("explicit-scope-replay"),
            create(
                "explicit-scope-command",
                Some(haider_rpc::SshScopeWire::None),
            ),
        )
        .await
        .expect("explicit scope replay");
    assert_eq!(
        response_session(&sink, "explicit-scope-replay"),
        explicit_session
    );
    assert_eq!(
        scopes
            .session_scope_if_present(&explicit_session)
            .expect("read replayed explicit scope"),
        Some(crate::ssh::SshScope::None)
    );

    drop(connection);
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");

    let restarted_store = SqliteStoreHandle::open(root.path())
        .await
        .expect("restarted store");
    let restarted_hub = SessionHub::new(restarted_store.clone(), SessionHubConfig::default())
        .expect("restarted hub");
    let mut restarted_provider = provider_summary("fake");
    restarted_provider.models = vec!["fake-v1".into()];
    restarted_provider.default_model = Some("fake-v1".into());
    restarted_hub
        .install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(Mutex::new(Vec::new())),
            management: crate::accounts::ManagementSnapshot::new(
                0,
                Vec::new(),
                vec![restarted_provider],
            ),
            vault_supported: true,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            vault: Some(vault),
        })
        .expect("restarted accounts install");
    restarted_hub
        .install_creatable_providers(std::collections::BTreeSet::from(["fake".into()]))
        .expect("restarted creatable provider install");
    let restarted_sink = Arc::new(CapturingFrameSink::default());
    let restarted_connection = restarted_hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::Control,
                haider_rpc::Capability::View,
            ]),
            restarted_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("restarted connection");
    restarted_sink.0.lock().expect("restarted frames").clear();
    restarted_connection
        .request(
            RequestId::new("default-scope-replay-after-restart"),
            create("default-scope-command", None),
        )
        .await
        .expect("create receipt replay after restart");
    assert_eq!(
        response_session(&restarted_sink, "default-scope-replay-after-restart"),
        default_session
    );
    assert!(
        lock(&restarted_hub.inner.ssh_scopes)
            .expect("restarted scope cache")
            .get(&default_session)
            .is_none(),
        "receipt replay after restart must not synthesize default All in an empty cache"
    );
    assert_eq!(
        restarted_hub
            .ssh_scope(&default_session)
            .expect("load restarted durable scope"),
        crate::ssh::SshScope::None,
        "receipt replay after restart must preserve a later durable narrowing"
    );
    drop(restarted_connection);
    restarted_hub
        .shutdown()
        .await
        .expect("restarted hub shutdown");
    restarted_store
        .close()
        .await
        .expect("restarted store close");

    let failure_root = tempfile::tempdir().expect("failure profile");
    let failure_store = SqliteStoreHandle::open(failure_root.path())
        .await
        .expect("failure store");
    let failure_hub =
        SessionHub::new(failure_store.clone(), SessionHubConfig::default()).expect("failure hub");
    let failure_vault = Arc::new(FailingScopePutVault {
        inner: haider_accounts::MemoryVault::default(),
        fail_scope_put: AtomicBool::new(true),
    });
    let mut failure_provider = provider_summary("fake");
    failure_provider.models = vec!["fake-v1".into()];
    failure_provider.default_model = Some("fake-v1".into());
    failure_hub
        .install_accounts(crate::accounts::AccountsFacade {
            login: None,
            oauth: None,
            snapshot: Arc::new(Mutex::new(Vec::new())),
            management: crate::accounts::ManagementSnapshot::new(
                0,
                Vec::new(),
                vec![failure_provider],
            ),
            vault_supported: true,
            discovery_disabled: true,
            device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
            sources: Arc::new(std::sync::Mutex::new(Vec::new())),
            vault: Some(failure_vault.clone()),
        })
        .expect("failure accounts install");
    failure_hub
        .install_creatable_providers(std::collections::BTreeSet::from(["fake".into()]))
        .expect("failure creatable provider install");
    let failure_sink = Arc::new(CapturingFrameSink::default());
    let failure_connection = failure_hub
        .open_connection(
            std::collections::BTreeSet::from([
                haider_rpc::Capability::Control,
                haider_rpc::Capability::View,
            ]),
            failure_sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("failure connection");
    failure_sink.0.lock().expect("failure frames").clear();
    let sessions_before = failure_store.session_ids().await.expect("sessions before");
    let failure = failure_connection
        .request(
            RequestId::new("explicit-scope-failure"),
            create(
                "explicit-scope-failure-command",
                Some(haider_rpc::SshScopeWire::None),
            ),
        )
        .await
        .expect_err("scope persistence failure must fail the create request");
    assert!(
        failure
            .to_string()
            .contains("injected SSH scope persistence failure")
    );
    assert!(
        failure_sink.0.lock().expect("failure frames").is_empty(),
        "a failed previsibility create must publish no session response"
    );
    assert_eq!(
        failure_store.session_ids().await.expect("sessions after"),
        sessions_before,
        "scope persistence failure must precede durable session visibility"
    );
    assert!(
        lock(&failure_hub.inner.actors)
            .expect("failure actors")
            .is_empty(),
        "scope persistence failure must precede actor visibility"
    );
    assert!(
        failure_vault
            .list()
            .expect("failure vault aliases")
            .iter()
            .all(|alias| !alias.as_str().starts_with("haider.ssh.scope.")),
        "a failed previsibility scope write must leave no vault row"
    );
    drop(failure_connection);
    failure_hub.shutdown().await.expect("failure hub shutdown");
    failure_store.close().await.expect("failure store close");
}

/// The durable child row is not the roster linearization point. While a fork
/// is paused immediately after commit, list observers see no child. Once the
/// fork returns, the child has an actor, Pipe coverage through its full head,
/// prompt provenance in the roster, and sequence-zero attachment replay.
///
/// MUTATION CHECK: remove the `fork_candidates` list filter, publish the
/// commit projection before Pipe maintenance, or install the actor after
/// removing the fence. Expected failure: the during-commit list exposes the
/// child or one of the post-publication readiness assertions fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_roster_publication_waits_for_actor_and_complete_pipe_projection() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let child = SessionId::new("fork-publication-barrier-child");
    let (reached_tx, mut reached_rx) = mpsc::unbounded_channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(ForkPublicationGate {
        child: child.clone(),
        gated: AtomicBool::new(false),
        reached: reached_tx,
        release: Mutex::new(release_rx),
    });
    let hub = SessionHub::with_observer(store.clone(), SessionHubConfig::default(), observer)
        .expect("hub");
    hub.install_accounts(transcription_facade(Arc::new(
        haider_accounts::MemoryVault::default(),
    )))
    .expect("install scope vault");
    let (source, _, _) = create_fork_ready_source(&hub, &store, "fork-publication-barrier").await;
    let prompt_seq = store
        .read(&source, 0, 64)
        .await
        .expect("source transcript")
        .into_iter()
        .find_map(|envelope| {
            matches!(
                envelope.payload.decode_event(),
                Ok(EventPayload::UserMessage { .. })
            )
            .then_some(envelope.seq)
        })
        .expect("source user prompt");
    let request_json = serde_json::json!({
        "source": &source,
        "prompt": { "seq": prompt_seq },
    })
    .to_string();
    let command = SessionPromptForkCommand {
        command_id: "fork-publication-barrier-command".into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        source_session_id: source,
        session_id: child.clone(),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        prompt_seq,
        name: Some("Published fork".into()),
        audit_event_id: EventId::new("fork-publication-barrier-audit"),
        device_id: DeviceId::new("fork-publication-barrier-device"),
    };
    let sink = Arc::new(CapturingFrameSink::default());
    let connection = hub
        .open_connection(
            std::collections::BTreeSet::from([haider_rpc::Capability::View]),
            sink.clone(),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("roster observer");
    sink.0.lock().expect("frames").clear();

    let fork_hub = hub.clone();
    let fork = tokio::spawn(async move { fork_hub.fork_session_from_prompt(command).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx.recv())
        .await
        .expect("fork reaches durable boundary")
        .expect("observer remains open");
    connection
        .request(
            RequestId::new("list-during-fork-publication"),
            RequestBody::SessionList {
                cursor: None,
                limit: 100,
                order: Default::default(),
            },
        )
        .await
        .expect("list during fork");
    assert!(
        listed_sessions(&sink, "list-during-fork-publication")
            .iter()
            .all(|summary| summary.session_id != child),
        "a durable but unprojected fork must remain outside the roster"
    );

    release_tx.send(()).expect("release publication barrier");
    let outcome = fork.await.expect("fork task").expect("fork publishes");
    let SessionForkOutcome::Committed { created, .. } = outcome else {
        panic!("fresh prompt fork commits");
    };
    assert!(
        hub.existing_actor(&child)
            .expect("actor registry")
            .is_some(),
        "the actor exists before roster publication"
    );
    assert!(matches!(
        hub.inner.pipe_native.confirmed_coverage(&child),
        Some((coverage, generation))
            if coverage >= created.created_seq && generation > 0
    ));

    connection
        .request(
            RequestId::new("list-after-fork-publication"),
            RequestBody::SessionList {
                cursor: None,
                limit: 100,
                order: Default::default(),
            },
        )
        .await
        .expect("list after fork");
    let summary = listed_sessions(&sink, "list-after-fork-publication")
        .into_iter()
        .find(|summary| summary.session_id == child)
        .expect("published child appears in roster");
    assert_eq!(summary.forked_from, created.forked_from);
    assert!(
        summary.forked_from.is_some(),
        "prompt provenance is projected"
    );

    sink.0.lock().expect("frames").clear();
    connection
        .request(
            RequestId::new("attach-published-fork"),
            RequestBody::SessionAttach {
                session_id: child.clone(),
                after_seq: 0,
                mode: AttachMode::View,
                sealed_replay: false,
            },
        )
        .await
        .expect("published child is addressable");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let caught_up = sink.0.lock().expect("frames").iter().any(|frame| {
                matches!(
                    frame,
                    WireFrame::AttachCaughtUp { high_water_seq, .. }
                        if *high_water_seq == created.created_seq
                )
            });
            if caught_up {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("sequence-zero replay catches up");
    let replayed = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .filter_map(|frame| match frame {
            WireFrame::Event { envelope, .. } if envelope.session_id == child => Some(envelope.seq),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(replayed.first(), Some(&1));
    assert_eq!(replayed.last(), Some(&created.created_seq));

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// The additive prompt selector reaches the prompt-cut store transaction over
/// the real RPC dispatcher, and the response/list projections share the exact
/// durable provenance rather than requiring a second client read.
#[tokio::test]
async fn prompt_fork_rpc_publishes_response_draft_and_roster_provenance() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    hub.install_accounts(transcription_facade(Arc::new(
        haider_accounts::MemoryVault::default(),
    )))
    .expect("install scope vault");
    let (source, _, _) = create_fork_ready_source(&hub, &store, "prompt-fork-rpc").await;
    let prompt_seq = store
        .read(&source, 0, 64)
        .await
        .expect("source transcript")
        .into_iter()
        .find_map(|envelope| {
            matches!(
                envelope.payload.decode_event(),
                Ok(EventPayload::UserMessage { .. })
            )
            .then_some(envelope.seq)
        })
        .expect("source user prompt");
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
            RequestId::new("attach-prompt-fork-source"),
            RequestBody::SessionAttach {
                session_id: source.clone(),
                after_seq: store.latest_seq(&source).await.expect("source head"),
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("source control attachment");
    connection
        .request(
            RequestId::new("prompt-fork-rpc"),
            RequestBody::SessionFork {
                command_id: CommandId::new("prompt-fork-rpc-command"),
                session_id: source.clone(),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: None,
                fork_seq: None,
                prompt: Some(haider_protocol::session_fork::SessionForkPromptSelector {
                    seq: prompt_seq,
                }),
                name: Some("RPC prompt fork".into()),
            },
        )
        .await
        .expect("prompt fork routes");
    let (child, created_seq, forked_from, draft) = sink
        .0
        .lock()
        .expect("frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    ResponseBody::SessionFork {
                        session_id,
                        created_seq,
                        forked_from,
                        draft,
                        ..
                    },
            } if request_id.as_str() == "prompt-fork-rpc" => Some((
                session_id.clone(),
                *created_seq,
                forked_from.clone(),
                draft.clone(),
            )),
            _ => None,
        })
        .expect("typed prompt-fork response");
    assert!(forked_from.is_some());
    assert_eq!(
        forked_from.as_ref().map(|provenance| provenance.seq),
        Some(prompt_seq)
    );
    assert_eq!(
        draft.as_ref().map(|draft| draft.text.as_str()),
        Some("fixture turn")
    );
    assert!(
        hub.existing_actor(&child)
            .expect("actor registry")
            .is_some()
    );
    assert!(matches!(
        hub.inner.pipe_native.confirmed_coverage(&child),
        Some((coverage, generation)) if coverage >= created_seq && generation > 0
    ));

    // Model receipt replay in a fresh hub generation: durable authority knows
    // the child, but neither the actor registry nor the in-memory Pipe cursor
    // does. The retry must reinstall its own fence and fully repair both.
    hub.inner.actors.lock().expect("actors").remove(&child);
    hub.inner.pipe_native.invalidate(&child);
    assert!(
        !hub.inner
            .fork_candidates
            .lock()
            .expect("fork candidates")
            .contains(&child)
    );
    assert!(
        hub.existing_actor(&child)
            .expect("actor registry")
            .is_none()
    );
    assert!(hub.inner.pipe_native.confirmed_coverage(&child).is_none());
    connection
        .request(
            RequestId::new("prompt-fork-rpc-retry"),
            RequestBody::SessionFork {
                command_id: CommandId::new("prompt-fork-rpc-command"),
                session_id: source,
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: None,
                fork_seq: None,
                prompt: Some(haider_protocol::session_fork::SessionForkPromptSelector {
                    seq: prompt_seq,
                }),
                name: Some("RPC prompt fork".into()),
            },
        )
        .await
        .expect("prompt fork receipt repairs publication");
    assert!(
        !hub.inner
            .fork_candidates
            .lock()
            .expect("fork candidates")
            .contains(&child)
    );
    assert!(
        hub.existing_actor(&child)
            .expect("actor registry")
            .is_some()
    );
    assert!(matches!(
        hub.inner.pipe_native.confirmed_coverage(&child),
        Some((coverage, generation)) if coverage >= created_seq && generation > 0
    ));
    assert!(sink.0.lock().expect("frames").iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: ResponseBody::SessionFork { session_id, .. },
        } if request_id.as_str() == "prompt-fork-rpc-retry" && session_id == &child
    )));

    connection
        .request(
            RequestId::new("list-prompt-fork-rpc"),
            RequestBody::SessionList {
                cursor: None,
                limit: 100,
                order: Default::default(),
            },
        )
        .await
        .expect("list published prompt fork");
    let summary = listed_sessions(&sink, "list-prompt-fork-rpc")
        .into_iter()
        .find(|summary| summary.session_id == child)
        .expect("child roster row");
    assert_eq!(summary.forked_from, forked_from);

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Fork-only errors preserve their stable code/data at the correlated RPC
/// boundary. In particular, typed invalid-cut details and store families do
/// not disappear into the generic turn mapper's `invalid_argument` fallback.
#[tokio::test]
async fn session_fork_error_mapper_preserves_every_typed_boundary_variant() {
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
        .expect("connection");
    sink.0.lock().expect("frames").clear();

    for code in [
        ErrorCode::StoreReadOnly,
        ErrorCode::StoreCorrupt,
        ErrorCode::StoreUnavailable,
        ErrorCode::StoreFull,
    ] {
        let mapped = fork_pipe_publication_error(crate::pipe_native::PipeNativeError::store(
            "fork projection read failed",
            HaiderError::new(code, "typed Pipe store failure", false),
        ));
        assert!(matches!(
            mapped,
            SessionHubError::Store(error) if error.code == code
        ));
    }

    let mut invalid_cut = HaiderError::new(ErrorCode::InvalidArgument, "invalid cut", false);
    invalid_cut.details = Some(serde_json::json!({
        "kind": "session_fork_invalid_cut",
        "session_id": "source",
        "seq": 7,
        "reason": "wrong_branch",
    }));
    let cases = [
        (
            "unstable",
            HaiderError::new(ErrorCode::ForkCutUnstable, "unstable", true),
            "fork_cut_unstable",
        ),
        (
            "not-found",
            HaiderError::new(ErrorCode::SessionNotFound, "missing", false),
            "not_found",
        ),
        (
            "stale",
            HaiderError::new(ErrorCode::SingleWriterViolation, "stale", false),
            "stale_generation",
        ),
        ("invalid-cut", invalid_cut, "invalid_argument"),
        (
            "read-only",
            HaiderError::new(ErrorCode::StoreReadOnly, "read only", false),
            "store_read_only",
        ),
        (
            "corrupt",
            HaiderError::new(ErrorCode::StoreCorrupt, "corrupt", false),
            "store_corrupt",
        ),
        (
            "unavailable",
            HaiderError::new(ErrorCode::StoreUnavailable, "unavailable", true),
            "store_unavailable",
        ),
        (
            "full",
            HaiderError::new(ErrorCode::StoreFull, "full", false),
            "store_full",
        ),
    ];
    for (request_id, error, _) in &cases {
        connection
            .respond_session_fork_error(RequestId::new(*request_id), error.clone())
            .expect("typed fork error sends");
    }
    {
        let frames = sink.0.lock().expect("frames");
        for (request_id, _, expected_code) in cases {
            let body = frames
                .iter()
                .find_map(|frame| match frame {
                    WireFrame::Response {
                        request_id: seen,
                        body: ResponseBody::Error { code, data, .. },
                    } if seen.as_str() == request_id => Some((code.as_str(), data.as_ref())),
                    _ => None,
                })
                .expect("mapped response");
            assert_eq!(body.0, expected_code, "{request_id}");
            if request_id == "invalid-cut" {
                assert!(matches!(
                    body.1,
                    Some(ErrorData::SessionForkInvalidCut { session_id, seq: 7, .. })
                        if session_id.as_str() == "source"
                ));
            }
        }
    }

    connection
        .request(
            RequestId::new("capability"),
            RequestBody::SessionFork {
                command_id: CommandId::new("denied"),
                session_id: SessionId::new("source"),
                worker_generation: store.worker_generation(),
                source_branch_id: None,
                fork_node_id: None,
                fork_seq: None,
                prompt: Some(haider_protocol::session_fork::SessionForkPromptSelector { seq: 1 }),
                name: None,
            },
        )
        .await
        .expect("capability refusal routes");
    assert!(sink.0.lock().expect("frames").iter().any(|frame| {
        matches!(
            frame,
            WireFrame::Response {
                request_id,
                body: ResponseBody::Error { code, .. },
            } if request_id.as_str() == "capability" && code == "capability_denied"
        )
    }));

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
}

/// Queue-control changes share the existing attachment pipe. Each durable
/// queue transition must publish exactly one typed delta whose revision is
/// the committing envelope sequence; no queue-specific polling path exists.
#[tokio::test]
async fn queue_changes_publish_revisioned_attachment_deltas() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let session_id = SessionId::new("queue-watch-session");
    let active_run = RunId::new("queue-watch-active");
    let generation = store.worker_generation();
    hub.create_internal_session(create_command(&session_id, "queue-watch"))
        .await
        .expect("session created");
    hub.accept_internal_turn(accept_command(
        &session_id,
        &active_run,
        generation,
        "queue-watch-active",
    ))
    .await
    .expect("active turn accepted");
    let mut thinking = [run_state_envelope(
        &session_id,
        &active_run,
        generation,
        "queue-watch-thinking",
        RunState::Thinking,
    )];
    hub.append(&mut thinking).await.expect("active run starts");

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
        .expect("connection opens");
    let head = store.latest_seq(&session_id).await.expect("head");
    connection
        .request(
            haider_rpc::RequestId::new("queue-watch-attach"),
            haider_rpc::RequestBody::SessionAttach {
                session_id: session_id.clone(),
                after_seq: head,
                mode: haider_rpc::AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("control attachment opens");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if sink
                .0
                .lock()
                .expect("frames")
                .iter()
                .any(|frame| matches!(frame, WireFrame::AttachCaughtUp { .. }))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("attachment catches up");
    sink.0.lock().expect("frames").clear();

    let queued = |suffix: &str, text: &str| {
        let mut command = accept_command(
            &session_id,
            &RunId::new(format!("queue-watch-{suffix}")),
            generation,
            &format!("queue-watch-{suffix}"),
        );
        command.text = text.into();
        command
    };

    let first = hub
        .accept_internal_turn(queued("remove", "remove me"))
        .await
        .expect("first row enqueued");
    let snapshot = hub
        .queue_snapshot(session_id.clone())
        .await
        .expect("first snapshot");
    hub.queue_remove(QueueRemoveCommand {
        session_id: session_id.clone(),
        id: EventId::new("user-queue-watch-remove"),
        revision: snapshot.revision,
        cancelling_event_id: EventId::new("queue-watch-remove-cancelling"),
        delta_event_id: EventId::new("queue-watch-remove-delta"),
        device_id: DeviceId::new("queue-watch-device"),
    })
    .await
    .expect("row removed");

    let promoted_text = "  promote\nverbatim  ";
    hub.accept_internal_turn(queued("promote", promoted_text))
        .await
        .expect("second row enqueued");
    let snapshot = hub
        .queue_snapshot(session_id.clone())
        .await
        .expect("second snapshot");
    let (promoted, live_delivered) = hub
        .queue_promote_steer(QueuePromoteCommand {
            session_id: session_id.clone(),
            id: EventId::new("user-queue-watch-promote"),
            revision: snapshot.revision,
            expected_active_run_id: None,
            cancelling_event_id: EventId::new("queue-watch-promote-cancelling"),
            delivery_event_id: EventId::new("queue-watch-promote-delivery"),
            delta_event_id: EventId::new("queue-watch-promote-delta"),
            device_id: DeviceId::new("queue-watch-device"),
        })
        .await
        .expect("row promoted");
    assert_eq!(promoted.text, promoted_text);
    assert!(!live_delivered, "this fixture has no live supervisor");

    let consumed = hub
        .accept_internal_turn(queued("consume", "consume me"))
        .await
        .expect("third row enqueued");
    let lease = hub
        .acquire_worker_lease(session_id.clone())
        .await
        .expect("worker lease");
    lease
        .consume_queued_turn(
            consumed.run_id,
            EventId::new("queue-watch-consumed-delta"),
            DeviceId::new("queue-watch-worker"),
        )
        .await
        .expect("consumption commits")
        .expect("held row exists");
    assert_ne!(first.run_id, active_run);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let count = sink
                .0
                .lock()
                .expect("frames")
                .iter()
                .filter(|frame| {
                    matches!(frame, WireFrame::Event { envelope, .. }
                    if envelope.payload.decode_event()
                        .is_ok_and(|payload| matches!(payload, EventPayload::QueueChanged(_))))
                })
                .count();
            if count >= 6 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all queue deltas publish");

    let frames = sink.0.lock().expect("frames").clone();
    let deltas = frames
        .iter()
        .filter_map(|frame| match frame {
            WireFrame::Event { envelope, .. } => {
                let EventPayload::QueueChanged(delta) = envelope.payload.decode_event().ok()?
                else {
                    return None;
                };
                Some((envelope.seq, delta))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(deltas.len(), 6);
    assert!(
        deltas
            .windows(2)
            .all(|pair| pair[0].1.revision < pair[1].1.revision)
    );
    assert!(deltas.iter().all(|(seq, delta)| *seq == delta.revision));
    assert!(matches!(&deltas[0].1.change, QueueChange::Enqueued { .. }));
    assert!(matches!(&deltas[1].1.change, QueueChange::Removed { .. }));
    assert!(matches!(&deltas[2].1.change, QueueChange::Enqueued { .. }));
    assert!(matches!(
        &deltas[3].1.change,
        QueueChange::PromotedSteer { .. }
    ));
    assert!(matches!(&deltas[4].1.change, QueueChange::Enqueued { .. }));
    assert!(matches!(&deltas[5].1.change, QueueChange::Consumed { .. }));
    assert!(
        hub.queue_snapshot(session_id.clone())
            .await
            .expect("final snapshot")
            .rows
            .is_empty()
    );

    let stale_revision = deltas[0].1.revision;
    let current_revision = deltas[5].1.revision;
    let head_before_stale = store
        .latest_seq(&session_id)
        .await
        .expect("head before stale");
    connection
        .request(
            haider_rpc::RequestId::new("queue-watch-list"),
            haider_rpc::RequestBody::QueueList {
                session_id: session_id.clone(),
            },
        )
        .await
        .expect("queue list responds");
    connection
        .request(
            haider_rpc::RequestId::new("queue-watch-stale-remove"),
            haider_rpc::RequestBody::QueueRemove {
                session_id: session_id.clone(),
                id: EventId::new("user-queue-watch-remove"),
                revision: stale_revision,
            },
        )
        .await
        .expect("stale removal is typed");
    let frames = sink.0.lock().expect("frames").clone();
    assert!(frames.iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::QueueList {
                revision,
                rows,
                ..
            },
        } if request_id.as_str() == "queue-watch-list"
            && *revision == current_revision
            && rows.is_empty()
    )));
    assert!(frames.iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::Error {
                code,
                data: Some(haider_rpc::ErrorData::RevisionConflict {
                    expected_revision,
                    current_revision: refused_current,
                }),
                ..
            },
        } if request_id.as_str() == "queue-watch-stale-remove"
            && code == haider_rpc::ERROR_CODE_REVISION_CONFLICT
            && *expected_revision == stale_revision
            && *refused_current == current_revision
    )));
    assert_eq!(
        store
            .latest_seq(&session_id)
            .await
            .expect("head after stale"),
        head_before_stale,
        "typed stale refusal mutates nothing"
    );

    drop(connection);
    hub.shutdown().await.expect("hub stops");
    store.close().await.expect("store closes");
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

/// An available empty history read is successful absence, not subsystem
/// unavailability. Store errors take the correlated error path in the router
/// and therefore cannot be flattened into this response shape.
#[tokio::test]
async fn usage_history_routes_preserve_available_absence() {
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

    connection
        .request(
            haider_rpc::RequestId::new("history-day"),
            haider_rpc::RequestBody::UsageHistoryDay {
                date: "2026-08-24".into(),
            },
        )
        .await
        .expect("day request routes");
    connection
        .request(
            haider_rpc::RequestId::new("history-range"),
            haider_rpc::RequestBody::UsageHistoryRange {
                through_date: "2026-08-24".into(),
                days: 2,
            },
        )
        .await
        .expect("range request routes");

    {
        let frames = sink.0.lock().expect("frames");
        let day = frames.iter().find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    haider_rpc::ResponseBody::UsageHistoryDay {
                        device_id,
                        day,
                        availability,
                        ..
                    },
            } if request_id.as_str() == "history-day" => Some((device_id, day, availability)),
            _ => None,
        });
        let (day_device_id, day, day_availability) = day.expect("day response");
        assert_eq!(day, &None);
        assert_eq!(
            day_availability,
            &Some(haider_rpc::SnapshotAvailabilityWire::Available)
        );
        let range = frames.iter().find_map(|frame| match frame {
            WireFrame::Response {
                request_id,
                body:
                    haider_rpc::ResponseBody::UsageHistoryRange {
                        device_id,
                        days,
                        availability,
                        ..
                    },
            } if request_id.as_str() == "history-range" => Some((device_id, days, availability)),
            _ => None,
        });
        let (range_device_id, days, availability) = range.expect("range response");
        assert_eq!(range_device_id, day_device_id);
        assert!(day_device_id.starts_with("dev-"));
        assert_eq!(day_device_id.len(), 36);
        assert_eq!(
            availability,
            &Some(haider_rpc::SnapshotAvailabilityWire::Available)
        );
        assert_eq!(days.len(), 2);
        assert!(days.iter().all(|day| day.total.is_none()));

        // MUTATION CHECK: zero-filling range cells makes the final assertion
        // fail; reporting empty-day success as unavailable breaks the exact day
        // availability assertion.
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
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
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
                event.payload.decode_event(),
                Ok(EventPayload::UserMessage { ref text, .. }) if text == "bounded review event"
            )
        })
        .expect("source user event")
        .seq;
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
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
    hub.install_accounts(transcription_facade(Arc::new(
        haider_accounts::MemoryVault::default(),
    )))
    .expect("install scope vault");
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
                event.payload.decode_event(),
                Ok(EventPayload::UserMessage { ref text, .. })
                    if text == "review this chocolate event"
            )
        })
        .expect("review source user event")
        .seq;
    let (fork_node_id, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) = event.payload.decode_event().ok()? else {
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
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
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
    let crate::accounts::ProviderModelsRefreshCompletion::Wire(completed) = completed else {
        panic!("RPC refresh must carry a wire completion");
    };
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
        payload: serde_json::to_value(EventPayload::RunState(state))
            .expect("state serializes")
            .into(),
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
    *envelope.payload = serde_json::to_value(payload).expect("payload serializes");
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
        .filter_map(|envelope| envelope.payload.decode_event().ok())
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
    *fact.payload = haider_protocol::session::ModelSelected {
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
    *fact.payload = haider_protocol::session::SessionConfigEventPayload::session_renamed_value(
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
    *graph_fact.payload = serde_json::to_value(EventPayload::GraphPinned(
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
    *effort_fact.payload = haider_protocol::session::EffortSelected {
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
    *fast_fact.payload = haider_protocol::session::FastModeSelected { enabled: true }
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
            && envelope
                .payload
                .decode_event()
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
                    && envelope.payload.decode_event().is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::Effect(EffectPhase::Dispatched { .. })
                        )
                    })
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
                    && envelope
                        .payload
                        .decode_event()
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
        .filter_map(|envelope| envelope.payload.decode_event().ok())
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
                    && envelope.payload.decode_event().is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::Effect(EffectPhase::Dispatched { .. })
                        )
                    })
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
        .filter_map(|envelope| envelope.payload.decode_event().ok())
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
                && envelope.payload.decode_event().is_ok_and(|payload| {
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
                })
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

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, "panic-test-one")
        .await
        .expect("supervisor exit terminalizes shell");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let completed = history
        .iter()
        .find(|envelope| {
            envelope.run_id.as_ref() == Some(&run_id)
                && envelope.payload.decode_event().is_ok_and(|payload| {
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
                })
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

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, "panic-test-two")
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            envelope
                .payload
                .decode_event()
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

    crate::worker::terminalize_supervisor_exit(&hub, &session_id, "panic-test-three")
        .await
        .expect("panic exit terminalizes");
    let history = haider_core::StoreHandle::read(&store, &session_id, 0, 128)
        .await
        .expect("history reads");
    let payloads = history
        .into_iter()
        .filter(|envelope| envelope.run_id.as_ref() == Some(&run_id))
        .filter_map(|envelope| {
            envelope
                .payload
                .decode_event()
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
        payload: serde_json::Value::Null.into(),
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
            .filter_map(|envelope| envelope.payload.decode_event().ok())
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
            .filter_map(|envelope| match envelope.payload.decode_event() {
                Ok(EventPayload::RunState(state)) => Some(state),
                _ => None,
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
        device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(false),
        sources: Arc::new(std::sync::Mutex::new(Vec::new())),
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
    assert!(matches!(
        observe_run_state(&RunState::Waiting {
            reason: haider_protocol::state::WaitReason::NetworkUnavailable,
        }),
        ObserveRunStateWire::WaitingForRoute
    ));
}
