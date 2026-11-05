#![allow(clippy::expect_used)]

//! Private fair-outbox, reply-floor, and drain-order tests.
//!
//! This module is crate-internal only because `OutboundLane`, `LaneKey`, and
//! `run_writer` intentionally are not production API.

// Keep this transport-specific suite unavailable off Unix even if its parent
// module declaration is accidentally broadened in the future.
#![cfg(unix)]

use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixStream, unix::OwnedWriteHalf};

struct FirstWriteGate {
    writer: OwnedWriteHalf,
    gate: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    started: Option<oneshot::Sender<Instant>>,
}

impl AsyncWrite for FirstWriteGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(started) = self.started.take() {
            let _ = started.send(Instant::now());
        }
        if let Some(gate) = self.gate.as_mut() {
            if gate.as_mut().poll(context).is_pending() {
                return Poll::Pending;
            }
            self.gate = None;
        }
        Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(context)
    }
}

async fn settle_connection_and_writer_tasks() {
    // `advance` yields only once after waking due timers. The timeout path
    // crosses two spawned tasks (connection -> writer), so keep this test
    // runnable while both hops settle; that also prevents paused-time
    // auto-advance to the next timer.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

/// A silent peer's fatal handshake-timeout frame must finish writing before
/// the connection task tears down its socket and releases its admission slot.
/// Paused time makes the exact deadline deterministic; the gated first write
/// makes the shutdown ordering observable without scheduler timing slack.
#[tokio::test(start_paused = true)]
async fn handshake_timeout_frame_precedes_teardown_and_slot_release() {
    let (_dir, hub) = liveness_hub().await;
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let (reader, writer) = server.into_split();
    let (write_started, mut started) = oneshot::channel();
    let release = Arc::new(Notify::new());
    let gated_writer = FirstWriteGate {
        writer,
        gate: Some(Box::pin(Arc::clone(&release).notified_owned())),
        started: Some(write_started),
    };
    let (writers, mut registered_writers) = mpsc::unbounded_channel();
    let mut context = connection_context(hub.clone(), writers);
    context.handshake_timeout = std::time::Duration::from_millis(150);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let connection_slot = Arc::clone(&slots);
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(async move {
        let permit = connection_slot
            .acquire_owned()
            .await
            .expect("connection slot semaphore remains open");
        let exit = serve_io(reader, gated_writer, context, drain_rx).await;
        drop(permit);
        exit
    });

    let registered_writer = registered_writers
        .recv()
        .await
        .expect("serve_io registers its writer during connection setup");
    let handshake_started = Instant::now();
    assert!(
        Arc::clone(&slots).try_acquire_owned().is_err(),
        "the connection task must hold the only slot after writer registration"
    );

    let before_deadline = std::time::Duration::from_millis(149);
    tokio::time::advance(before_deadline).await;
    settle_connection_and_writer_tasks().await;
    assert_eq!(
        Instant::now().duration_since(handshake_started),
        before_deadline,
        "scheduler settling must not auto-advance the paused clock"
    );
    assert!(
        matches!(started.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "the timeout frame must not start writing before the 150ms deadline"
    );
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    settle_connection_and_writer_tasks().await;
    let write_started_at = started
        .try_recv()
        .expect("the timeout frame write starts at the handshake deadline");
    assert_eq!(
        write_started_at.duration_since(handshake_started),
        std::time::Duration::from_millis(150),
        "the timeout frame write must start at exactly the handshake deadline"
    );
    assert!(
        !serve_task.is_finished(),
        "connection teardown must wait for the gated timeout-frame write"
    );

    release.notify_one();
    let mut prefix = [0_u8; 4];
    client
        .read_exact(&mut prefix)
        .await
        .expect("timeout frame prefix reads");
    let body_len = usize::try_from(u32::from_be_bytes(prefix)).expect("frame length fits usize");
    let mut encoded = prefix.to_vec();
    encoded.resize(4 + body_len, 0);
    client
        .read_exact(&mut encoded[4..])
        .await
        .expect("complete timeout frame body reads");
    let mut decoder = uds_codec::Decoder::new(1024 * 1024);
    let decoded = decoder.push(&encoded);
    assert!(
        decoded.error.is_none(),
        "complete timeout frame must decode; got {:?}",
        decoded.error
    );
    assert_eq!(
        decoded.frames,
        vec![WireFrame::ProtocolError(ProtocolError {
            code: "handshake_timeout".into(),
            message: "Hello did not arrive before the handshake deadline".into(),
            fatal: true,
            presentation: None,
            failed_write_ids: Vec::new(),
        })]
    );

    let mut trailing = [0_u8; 1];
    assert_eq!(
        client
            .read(&mut trailing)
            .await
            .expect("read EOF after timeout frame"),
        0,
        "timeout frame must be complete before socket EOF"
    );
    assert_eq!(
        serve_task
            .await
            .expect("connection task joins")
            .expect("connection serves through timeout teardown"),
        ConnectionExit::ClosedBeforeDrain {
            reason: ConnectionRetirementReason::Error,
        }
    );
    let _reacquired = Arc::clone(&slots)
        .try_acquire_owned()
        .expect("connection slot is immediately reusable after task join");
    registered_writer.await.expect("registered writer joins");
    hub.shutdown().await.expect("hub shutdown");
}

fn ordinary(bytes: &[u8]) -> QueuedFrame {
    QueuedFrame::ordinary(bytes.to_vec())
}

fn staged_response(attachment: &AttachmentId, request: &str, bytes: &[u8]) -> QueuedFrame {
    QueuedFrame {
        bytes: bytes.to_vec().into(),
        welcome: false,
        response_for: Some((attachment.clone(), RequestId::new(request))),
        floor: false,
    }
}

/// The production handshake feature set includes every management family
/// served by W5c.2b and the already-merged OAuth and rotation work.
///
/// MUTATION CHECK: remove `FEATURE_ACCOUNT_OAUTH_IMPORT_V1` from
/// `welcome_features`. Expected runtime failure: the exact feature-set
/// assertion reports that `account_oauth_import_v1` is missing.
///
/// MUTATION CHECK: remove `FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1` from
/// `welcome_features`. Expected runtime failure: the exact feature-set
/// assertion reports that `account_oauth_import_sources_v1` is missing.
///
/// MUTATION CHECK: remove `FEATURE_CONTEXT_COMPACTION_V1`. Expected runtime
/// failure: clients cannot discover the served `session.compact` method.
/// Verified by revert on 2026-07-30.
///
/// MUTATION CHECK: remove `FEATURE_COMMAND_DOOR_V1` from `welcome_features`.
/// Runtime failure: the exact served-feature assertion reports
/// `command_door_v1` missing while both command methods remain routable.
///
/// MUTATION CHECK: remove `FEATURE_SHELL_EXEC_V1`, `FEATURE_USER_COMMAND_V1`,
/// or `FEATURE_TOOL_INVENTORY_V1`. Expected runtime failure: the exact feature
/// set no longer advertises the W8a RPC or its context/cancel semantics.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_PERMISSION_OVERRIDES_V1`.
/// Expected RUNTIME failure: headless clients cannot discover the durable
/// allow-writes/allow-exec create seam.
/// MUTATION CHECK: remove `FEATURE_SESSION_READ_ONLY_V1`. Expected RUNTIME
/// failure: a client cannot prove that an explicit read-only deny will be
/// understood rather than ignored by an older daemon.
///
/// MUTATION CHECK: remove `FEATURE_PROVIDER_REMOVE_V1`. Expected RUNTIME
/// failure: discovery omits the served durable provider removal method.
///
/// MUTATION CHECK: remove `FEATURE_BRANCH_CREATE_V1`. Expected RUNTIME
/// failure: clients cannot discover the served durable branch-create method.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_FORK_V1`. Expected RUNTIME failure:
/// clients cannot discover the served session-level fork/metafork doors.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_PROMPT_FORK_V1`. Expected RUNTIME
/// failure: clients cannot discover the additive exclusive-prompt selector,
/// draft response, or roster provenance field.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_OBSERVE_V1`. Expected RUNTIME
/// failure: scriptable clients cannot discover the served state digest.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_FLEET_V1`. Expected RUNTIME
/// failure: fleet clients cannot discover the served descendant snapshot.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_DESCENDANT_STREAM_V1`. Expected
/// RUNTIME failure: clients cannot discover the reconnectable nested stream
/// and may incorrectly treat the point-in-time fleet snapshot as live.
///
/// MUTATION CHECK: remove `FEATURE_HOOKS_V1`. Expected RUNTIME failure:
/// hook-aware clients cannot discover the served list/trust/run grant seam.
///
/// MUTATION CHECK: remove `FEATURE_AGENT_MESSAGE_V1`. Expected RUNTIME
/// failure: the future chip composer cannot discover its served wire.
///
/// MUTATION CHECK: remove `FEATURE_AGENT_CANCEL_V1`. Expected RUNTIME
/// failure: fleet clients cannot discover the owned direct-child cancellation
/// door and must not render destructive control against an older daemon.
///
/// MUTATION CHECK: remove `FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1`. Expected
/// RUNTIME failure: clients cannot discover the served metadata-only device
/// credential discovery + candidate import surface (D1).
///
/// MUTATION CHECK: remove `FEATURE_SESSION_MODEL_SELECT_V1`. Expected
/// RUNTIME failure: pickers cannot discover the served receipted
/// cross-provider model selection (F1).
///
/// MUTATION CHECK: remove `FEATURE_TRANSCRIPTION_V1`. Expected RUNTIME
/// failure: the TUI talk surface cannot discover the served vaulted
/// Deepgram-key RPCs (T1).
/// MUTATION CHECK: remove `FEATURE_USAGE_REPORT_V1`. Expected RUNTIME
/// failure: clients cannot discover the served cross-provider `usage.report`
/// snapshot (U1).
///
/// MUTATION CHECK: remove `FEATURE_USAGE_HISTORY_V1`. Expected RUNTIME
/// failure: clients cannot discover the two served device-local history reads.
///
/// MUTATION CHECK: remove `FEATURE_QUEUE_CONTROL_V1`. Expected RUNTIME
/// failure: clients cannot distinguish an unsupported queue-control surface
/// from a supported session whose held-message snapshot is empty.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_RENAME_V1`. Expected RUNTIME
/// failure: clients cannot discover the served receipted `session.rename`
/// surface (G2) and the TUI keeps its stale-daemon notice forever.
///
/// MUTATION CHECK: remove `FEATURE_RUN_RETRY_V1`. Expected RUNTIME failure:
/// clients cannot discover the served receipt-backed `run.retry` command.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_CONFIG_V1` or
/// `FEATURE_MODELS_LIST_V1`. Expected RUNTIME failure: the ADE cannot sniff
/// the headless session-config door / model-library enumeration (W-CFG).
///
/// MUTATION CHECK: remove `FEATURE_MODEL_OUTPUT_LIMITS_V1`. Expected RUNTIME
/// failure: headless clients cannot safely use the model-derived zero sentinel
/// and may reconnect to an older daemon that still rejects it.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_RUN_ID_V1`. Expected RUNTIME
/// failure: clients cannot discover that observation surfaces report the
/// active run id, leaving every session started elsewhere uncancellable
/// from any surface but the one that submitted it (W-flow).
///
/// MUTATION CHECK: remove `FEATURE_LOOM_CLI_PRESENCE_V1`. Expected RUNTIME
/// failure: clients cannot discover that `loom.list` reports device PATH
/// presence, so a missing declared CLI stays invisible until the agent
/// type's first failing turn (W-flow).
///
/// MUTATION CHECK: remove `FEATURE_TUI_ATTACH_ANNOUNCE_V1` or
/// `FEATURE_SESSION_LINEAGE_V1`. Expected RUNTIME failure: the ADE cannot
/// sniff the OSC 7791 attach announce stream / typed subagent lineage and
/// falls back to guessing PTY bindings and grepping id prefixes.
///
/// MUTATION CHECK: remove `FEATURE_RESIDENT_SESSION_BINDING_V1` from
/// `welcome_features`. Expected runtime failure: web clients keep scraping
/// OSC 7791 because the typed generation-fenced binding signal appears
/// unavailable even though the daemon serves it.
///
/// MUTATION CHECK: remove `FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1` from
/// `welcome_features`. Expected runtime failure: multi-surface clients cannot
/// discover the served client-originated token echo and keep scraping their
/// terminals for per-pane correlation.
///
/// MUTATION CHECK: remove `FEATURE_HAIDER_CODE_PLAN_STATUS_V1`. Expected
/// runtime failure: clients cannot discover the typed provider-plan stream
/// and would have to guess account health from percentages.
///
/// MUTATION CHECK: remove `FEATURE_PIPE_TOOL_STATUS_V1` from
/// `welcome_features`. Expected runtime failure: cold-history clients cannot
/// discover the typed tool outcome and fall back to parsing summary prose.
///
/// MUTATION CHECK: remove `FEATURE_MONITOR_CONTROL_V1`,
/// `FEATURE_MONITOR_MUTATE_V1`, or `FEATURE_MONITOR_DELIVERY_V1`. Expected
/// runtime failure: clients cannot distinguish the served typed base control,
/// mutation, and replay surfaces from the private APK transport or from total
/// client-surface absence.
///
/// MUTATION CHECK: remove `FEATURE_WORKFLOW_CATALOG_V1` or
/// `FEATURE_LOOM_PIPE_DAG_V1`. Expected runtime failure: clients cannot
/// discover the catalog snapshot or distinguish v0.0.961 fork/join/back-edge
/// grammar from legacy `loom_v1`.
///
/// MUTATION CHECK: remove `FEATURE_WORKFLOW_GRAPH_V1`. Expected runtime
/// failure: clients cannot discover the indexed activation state and
/// cursor-replay surfaces served by this daemon.
///
/// MUTATION CHECK: remove `FEATURE_SSH_PROFILES_V1` or
/// `FEATURE_SHELL_REGISTRY_V1`. Expected runtime failure: clients expose a
/// management or terminal-registry surface that the handshake says is absent.
///
/// MUTATION CHECK: remove `FEATURE_SSH_TYPED_SETUP_ERRORS_V1`. Expected
/// runtime failure: clients cannot distinguish typed setup errors from legacy
/// text-only setup errors after an uncoded command exit.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_FLEET_IDENTITY_V1`. Expected
/// RUNTIME failure: clients cannot distinguish manifest-backed name/model/
/// provider rows from the legacy fleet shape.
#[test]
fn welcome_features_pin_served_management_families() {
    eprintln!(
        "handshake pin: served_features={}",
        welcome_features().len()
    );
    assert_eq!(
        welcome_features().len(),
        125,
        "merged stop and typed SSH capabilities extend the served feature set"
    );
    assert_eq!(
        welcome_features(),
        BTreeSet::from([
            haider_rpc::FEATURE_AGENT_CANCEL_V1.to_owned(),
            haider_rpc::FEATURE_RUN_STOP_SCOPE_V1.to_owned(),
            haider_rpc::FEATURE_HELPER_STOP_V1.to_owned(),
            haider_rpc::FEATURE_AGENT_MESSAGE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned(),
            FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_SOURCES_V1.to_owned(),
            FEATURE_ACCOUNT_ROTATION_V1.to_owned(),
            haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned(),
            haider_rpc::binary_artifact::FEATURE.to_owned(),
            haider_rpc::FEATURE_EXPORT_SEQ_V1.to_owned(),
            haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FORK_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1.to_owned(),
            FEATURE_COMMAND_DOOR_V1.to_owned(),
            haider_rpc::FEATURE_COMPACTION_GUARD_V1.to_owned(),
            FEATURE_CONTEXT_COMPACTION_V1.to_owned(),
            haider_rpc::FEATURE_EFFECT_RECOVERY_V1.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V2.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V3.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V4.to_owned(),
            haider_rpc::FEATURE_HOOKS_SERVER_V1.to_owned(),
            FEATURE_HOOKS_V1.to_owned(),
            haider_rpc::FEATURE_FALLBACK_CHAIN_V1.to_owned(),
            haider_rpc::FEATURE_HEADLESS_RUN_V1.to_owned(),
            haider_rpc::FEATURE_AGENT_CLI_V1.to_owned(),
            haider_rpc::FEATURE_HAIDER_CODE_PLAN_STATUS_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_AUTHORING_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_CLI_PRESENCE_V1.to_owned(),
            haider_rpc::FEATURE_TYPED_AGENT_INSTALL_V1.to_owned(),
            haider_rpc::FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_REGISTRY_CAS_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_REGISTRY_ARCHIVE_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_VALIDATION_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_REGISTRY_WATCH_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_RUN_ID_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_PIPE_DAG_V1.to_owned(),
            haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
            haider_rpc::FEATURE_WORKFLOW_GRAPH_V1.to_owned(),
            haider_rpc::FEATURE_WORKFLOW_INSTANCE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_WORKFLOW_STATE_V1.to_owned(),
            haider_rpc::FEATURE_MODELS_LIST_V1.to_owned(),
            haider_rpc::FEATURE_PIPE_NATIVE_V2.to_owned(),
            haider_rpc::FEATURE_PIPE_TOOL_STATUS_V1.to_owned(),
            haider_rpc::FEATURE_INPUT_MIRROR_ATTACHMENTS_V1.to_owned(),
            haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_ATTACH_SEALED_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_CLOSE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LAUNCH_ORIGIN_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_CONFIG_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_CREATE_ADMISSION_V1.to_owned(),
            haider_rpc::FEATURE_MODEL_OUTPUT_LIMITS_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LIST_RECENCY_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LIST_WATCH_V1.to_owned(),
            haider_rpc::FEATURE_WIRE_MSGPACK_V1.to_owned(),
            FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
            FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
            haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1.to_owned(),
            FEATURE_PROVIDER_MODELS_V1.to_owned(),
            haider_rpc::FEATURE_PROVIDER_MODELS_PROBE_V1.to_owned(),
            FEATURE_PROVIDER_REMOVE_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_SESSION_BINDING_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1.to_owned(),
            haider_rpc::FEATURE_RUN_RETRY_V1.to_owned(),
            haider_rpc::FEATURE_RUN_BUDGET_V1.to_owned(),
            haider_rpc::FEATURE_REQUEST_BUDGET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_PROVIDER_REBIND_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned(),
            FEATURE_SESSION_MUTATION_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_RENAME_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_NEEDS_INPUT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_LIST_WATCH_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_LABEL_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FLEET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FLEET_IDENTITY_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_DESCENDANT_STREAM_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_V1.to_owned(),
            FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
            FEATURE_SESSION_READ_ONLY_V1.to_owned(),
            haider_rpc::FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_ACCOUNT_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_SEGMENT_STRUCTURED_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_SEGMENT_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_RUNTIME_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_SNAPSHOT_V1.to_owned(),
            haider_rpc::FEATURE_STORE_HEALTH_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_BATCH_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_TURN_SUBMIT_V1.to_owned(),
            haider_rpc::FEATURE_TUI_ATTACH_ANNOUNCE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LINEAGE_V1.to_owned(),
            FEATURE_SHELL_EXEC_V1.to_owned(),
            FEATURE_SSH_PROFILES_V1.to_owned(),
            FEATURE_SSH_TYPED_SETUP_ERRORS_V1.to_owned(),
            FEATURE_SHELL_REGISTRY_V1.to_owned(),
            haider_rpc::FEATURE_USER_COMMAND_V1.to_owned(),
            FEATURE_TOOL_INVENTORY_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_MUTATE_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_DELIVERY_V1.to_owned(),
            haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned(),
            FEATURE_TURN_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_TURN_RETRACT_V1.to_owned(),
            haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned(),
            haider_rpc::FEATURE_USAGE_HISTORY_V1.to_owned(),
            haider_rpc::FEATURE_QUEUE_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_PEER_MESSAGING_V1.to_owned(),
            haider_rpc::FEATURE_PEER_AGENT_INJECTION_V1.to_owned(),
            haider_rpc::FEATURE_COMPUTER_PERMISSION_ACTIONS_V1.to_owned(),
            FEATURE_VAULT_STAGE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_IDENTITY_V1.to_owned(),
            haider_rpc::FEATURE_CHECKPOINT_V1.to_owned(),
        ])
    );
}

#[test]
fn activity_token_requires_hello_request_and_follows_newer_withholding() {
    let mut hello = Hello {
        protocol_min: 1,
        protocol_max: 1,
        client_name: "released-client".into(),
        client_version: "0.0.972".into(),
        client_instance_id: "instance".into(),
        client_kind: haider_rpc::ClientKind::Tui,
        capabilities_requested: [Capability::View].into_iter().collect(),
        features_requested: Default::default(),
        max_receive_frame: 1_048_576,
        encodings: Vec::new(),
    };
    assert!(!welcome_features_for(&hello).contains(FEATURE_SESSION_ACTIVITY_V1));
    for near_miss in [
        "Session_Activity_V1",
        " session_activity_v1",
        "session_activity_v1 ",
        "session-activity-v1",
        "session_activity_v",
        "session_activity_v10",
        "sеssion_activity_v1", // Cyrillic e is not the ASCII token.
        "",
    ] {
        hello.features_requested.insert(near_miss.to_owned());
        assert!(!welcome_features_for(&hello).contains(FEATURE_SESSION_ACTIVITY_V1));
        hello.features_requested.clear();
    }
    hello
        .features_requested
        .insert(FEATURE_SESSION_ACTIVITY_V1.to_owned());
    hello.features_requested.insert("future_feature_v1".into());
    let features = welcome_features_for(&hello);
    assert!(!features.contains("future_feature_v1"));
    assert!(features.contains(FEATURE_SESSION_ACTIVITY_V1));
    assert!(features.contains(FEATURE_SESSION_FLEET_IDENTITY_V1));
    let welcome = Welcome {
        protocol: 1,
        instance_id: "daemon".into(),
        daemon_generation: 1,
        frame_limit: 1_048_576,
        profile_id: "profile".into(),
        daemon_version: "0.0.973".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: [Capability::View].into_iter().collect(),
        features,
        user_command_withheld: false,
        encoding: None,
    };
    let full_len = uds_codec::encode(&WireFrame::Welcome(welcome.clone()), usize::MAX)
        .expect("full Welcome")
        .len()
        - 4;
    let granted = encode_welcome_for_peer(welcome.clone(), full_len).expect("grant fits");
    assert!(granted.features.contains(FEATURE_SESSION_ACTIVITY_V1));
    let newest_withheld =
        encode_welcome_for_peer(welcome.clone(), full_len - 1).expect("newest token withheld");
    assert!(
        !newest_withheld
            .features
            .contains(FEATURE_SSH_TYPED_SETUP_ERRORS_V1)
    );
    assert!(
        newest_withheld
            .features
            .contains(FEATURE_SESSION_ACTIVITY_V1)
    );
    let mut retained = newest_withheld.features.clone();
    let mut next_limit = newest_withheld.bytes.len() - 5;
    for newer in [
        haider_rpc::FEATURE_HELPER_STOP_V1,
        haider_rpc::FEATURE_RUN_STOP_SCOPE_V1,
    ] {
        let next = encode_welcome_for_peer(welcome.clone(), next_limit)
            .expect("next merged token withheld before activity");
        assert!(retained.remove(newer));
        assert_eq!(next.features, retained, "withholding {newer}");
        assert!(next.features.contains(FEATURE_SESSION_ACTIVITY_V1));
        next_limit = next.bytes.len() - 5;
    }
    let withheld = encode_welcome_for_peer(welcome, next_limit)
        .expect("activity withheld after all newer tokens");
    assert!(!withheld.features.contains(FEATURE_SESSION_ACTIVITY_V1));
    assert!(
        withheld
            .features
            .contains(FEATURE_SESSION_FLEET_IDENTITY_V1)
    );
    let summary: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "child", "head_seq": 1, "worker_generation": 1
    }))
    .expect("summary");
    let frame = WireFrame::SessionRosterDelta {
        summaries: vec![haider_rpc::SessionSummary {
            owner_agent_id: None,
            launch_origin: None,
            agent_metrics: Some(identity_gate_metrics()),
            ..summary
        }],
    };
    for (features, has_activity) in [
        (granted.features, true),
        (newest_withheld.features, true),
        (withheld.features, false),
    ] {
        for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
            let sink = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: features.contains(FEATURE_SESSION_FLEET_IDENTITY_V1),
                activity: features.contains(FEATURE_SESSION_ACTIVITY_V1),
            };
            let bytes = outbound_bytes(sink.encode(&frame).expect("roster encodes"));
            assert_eq!(
                bytes.windows(8).any(|window| window == b"activity"),
                has_activity
            );
        }
    }
}

#[test]
fn welcome_advertises_typed_pipe_tool_status() {
    assert!(
        welcome_features().contains(haider_rpc::FEATURE_PIPE_TOOL_STATUS_V1),
        "the daemon publishes typed native-pipe tool status"
    );
}

#[test]
fn welcome_advertises_the_961_loom_pipe_dag_grammar() {
    assert!(
        welcome_features().contains(haider_rpc::FEATURE_LOOM_PIPE_DAG_V1),
        "fork/join/back-edge parsing requires its distinct negotiation bit"
    );
    let workflow = haider_protocol::loom::compile_pipe(
        &haider_protocol::loom::parse_pipe(
            "dag: A -> A\nroot\nleft <-root\nright <-root\njoin <-left,right ↺root",
        ),
        |_| None,
    )
    .expect("the advertised v0.0.961 DAG grammar compiles");
    let join = workflow.template.nodes.last().expect("join node");
    assert_eq!(join.depends_on.len(), 2);
    assert_eq!(
        join.red_target.as_ref().map(|node| node.as_str()),
        Some("ROOT")
    );
}

#[test]
fn welcome_advertises_oauth_import_source_catalog() {
    assert!(
        welcome_features().contains(haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1),
        "the daemon serves account.oauth_import_sources and must advertise it"
    );
}

#[test]
fn welcome_advertises_resident_session_binding_token_echo() {
    assert!(
        welcome_features().contains(FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1),
        "the daemon serves client-originated resident binding-token echo"
    );
}

// Literal reply bodies and byte ceilings emitted by 56e4a29a's encoder, before
// ssh_typed_setup_errors_v1 existed. Never build the predecessor from today's
// feature list: that would let an additive token move every compatibility limit.
// Never regenerate welcome-wave56.json from a later tree, including for a
// release-identity bump: tests that compare against the real daemon's own
// serialisation substitute only the pinned daemon_version (see
// `pinned_reply_for_current_daemon`).
#[derive(serde::Deserialize)]
struct PreviousWelcome {
    activity: bool,
    control: bool,
    encoding: Option<String>,
    withheld_count: usize,
    limit: usize,
    reply: String,
}

fn previous_welcomes() -> Vec<PreviousWelcome> {
    let rows: Vec<PreviousWelcome> = serde_json::from_str(include_str!("welcome-wave56.json"))
        .expect("pinned wave Welcome bytes");
    assert_eq!(rows.len(), 64);
    rows
}

fn previous_welcome(row: &PreviousWelcome) -> Welcome {
    assert_eq!(row.reply.len(), row.limit, "literal previous byte ceiling");
    let welcome: Welcome = serde_json::from_str(&row.reply).expect("previous Welcome");
    assert!(!welcome.features.contains(FEATURE_SSH_TYPED_SETUP_ERRORS_V1));
    assert_eq!(welcome.user_command_withheld, row.withheld_count == 7);
    welcome
}

fn current_welcome(row: &PreviousWelcome) -> Welcome {
    let mut welcome = previous_welcome(row);
    welcome.features = welcome_features();
    if row.activity {
        welcome
            .features
            .insert(FEATURE_SESSION_ACTIVITY_V1.to_owned());
    }
    welcome.user_command_withheld = false;
    welcome
}

/// The version every wave-56 row was pinned with. The fixture stays literal.
const WAVE56_DAEMON_VERSION_FIELD: &str = "\"daemon_version\":\"0.0.972\"";

/// The pinned reply as today's daemon serialises it: exactly one substitution,
/// the pinned `daemon_version` for this crate's version. The real dispatch path
/// writes `env!("CARGO_PKG_VERSION")`, so a version bump must not move the
/// expected bytes or their ceiling; every other byte stays literal.
fn pinned_reply_for_current_daemon(reply: &str) -> String {
    assert_eq!(
        reply.matches(WAVE56_DAEMON_VERSION_FIELD).count(),
        1,
        "each pinned reply carries the pinned daemon_version exactly once"
    );
    reply.replacen(
        WAVE56_DAEMON_VERSION_FIELD,
        &format!("\"daemon_version\":\"{}\"", env!("CARGO_PKG_VERSION")),
        1,
    )
}

fn pinned_welcome_bytes(body: &str) -> Vec<u8> {
    let mut bytes = u32::try_from(body.len())
        .expect("fixture length")
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// Killed mutations: never withhold the new token; withhold it after an older
/// capability; derive expected bytes from the current set. The fixture is fixed
/// independently of welcome_features(), including its eight old boundaries.
#[test]
fn tight_welcome_preserves_all_64_wave56_replies() {
    let mut failures = Vec::new();
    for row in previous_welcomes() {
        let expected = pinned_welcome_bytes(&row.reply);
        // An actual old feature list remains a negative control.
        let old = encode_welcome_for_peer(previous_welcome(&row), row.limit)
            .expect("unchanged previous reply fits");
        assert_eq!(old.bytes.as_slice(), expected);
        let result = encode_welcome_for_peer(current_welcome(&row), row.limit);
        let exact = result.as_ref().is_ok_and(|encoded| {
            encoded.bytes.as_slice() == expected
                && encoded.features == previous_welcome(&row).features
        });
        println!(
            "previous Welcome: encoding={:?} activity={} control={} boundary={} limit={} preserved={exact}",
            row.encoding, row.activity, row.control, row.withheld_count, row.limit
        );
        if !exact {
            failures.push((row.encoding, row.activity, row.control, row.withheld_count));
        }
    }
    assert!(
        failures.is_empty(),
        "previous connection capability list changed: {failures:?}"
    );
    assert_unadvertised_fleet_identity_is_byte_identical();
}

/// Each merged additive token must leave the preceding capabilities intact.
/// Activity was already supported by wave 56 and cannot be removed first.
#[test]
fn tight_welcome_withholds_new_stop_and_ssh_tokens_before_wave56_features() {
    for row in previous_welcomes()
        .into_iter()
        .filter(|row| row.withheld_count == 0)
    {
        let current = current_welcome(&row);
        let full = uds_codec::encode(&WireFrame::Welcome(current.clone()), usize::MAX)
            .expect("full merged Welcome");
        let ample = encode_welcome_for_peer(current.clone(), full.len() - 4)
            .expect("ample peer retains all merged capabilities");
        assert_eq!(ample.bytes.as_slice(), full);
        assert_eq!(ample.features, current.features);

        let mut preceding = current.clone();
        let mut preceding_len = full.len() - 4;
        for token in [
            FEATURE_SSH_TYPED_SETUP_ERRORS_V1,
            haider_rpc::FEATURE_HELPER_STOP_V1,
            haider_rpc::FEATURE_RUN_STOP_SCOPE_V1,
        ] {
            assert!(preceding.features.remove(token));
            let expected = uds_codec::encode(&WireFrame::Welcome(preceding.clone()), usize::MAX)
                .expect("preceding merged Welcome");
            let encoded = encode_welcome_for_peer(current.clone(), preceding_len - 1)
                .expect("tight peer retains preceding capabilities");
            assert_eq!(encoded.bytes.as_slice(), expected, "withholding {token}");
            assert_eq!(encoded.features, preceding.features);
            preceding_len = expected.len() - 4;
        }
        assert_eq!(preceding, previous_welcome(&row));
    }
}

#[test]
fn tight_welcome_preserves_non_ready_wave56_replies_and_ample_peers() {
    for row in previous_welcomes() {
        let current = current_welcome(&row);
        let full = uds_codec::encode(&WireFrame::Welcome(current.clone()), usize::MAX)
            .expect("ample Welcome");
        let ample = encode_welcome_for_peer(current.clone(), full.len() - 4).expect("ample peer");
        assert_eq!(ample.bytes.as_slice(), full);
        assert!(ample.features.contains(FEATURE_SSH_TYPED_SETUP_ERRORS_V1));
        for (phase, name) in [
            (LifecyclePhase::Starting, "starting"),
            (LifecyclePhase::Recovering, "recovering"),
            (LifecyclePhase::Draining, "draining"),
            (LifecyclePhase::Finalizing, "finalizing"),
            (LifecyclePhase::Stopped, "stopped"),
            (LifecyclePhase::Failed, "failed"),
            (LifecyclePhase::Unknown, "unknown"),
        ] {
            // Only the lifecycle spelling changes in these literal old bytes.
            let body = row.reply.replace(
                "\"lifecycle_phase\":\"ready\"",
                &format!("\"lifecycle_phase\":\"{name}\""),
            );
            assert_ne!(body, row.reply);
            let mut welcome = current.clone();
            welcome.lifecycle_phase = phase;
            let encoded =
                encode_welcome_for_peer(welcome, body.len()).expect("non-Ready old ceiling");
            assert_eq!(
                encoded.bytes.as_slice(),
                pinned_welcome_bytes(&body),
                "{name}"
            );
        }
        if row.withheld_count == 7 {
            assert!(
                encode_welcome_for_peer(current, row.limit - 1).is_err(),
                "a peer below the shipped minimum still cannot connect"
            );
        }
    }
}

#[tokio::test]
async fn tight_welcome_dispatch_preserves_wave56_bytes_with_cold_and_warm_cache() {
    let (_root, hub) = liveness_hub().await;
    let mut context = liveness_context(hub);
    context.instance_id = "local-instance".into();
    context.profile_id = "local-profile".into();
    let (_sender, drain) = watch::channel(None);
    // Tight first, then ample, then tight again: a cached full reply must not
    // displace old capabilities, and a tight reply must never poison the cache.
    for ample in [false, true, false] {
        for row in previous_welcomes() {
            // The peer limit is the ceiling of the reply this daemon would have
            // sent, sized from the substituted body, not from today's set.
            let reply = pinned_reply_for_current_daemon(&row.reply);
            let lane = OutboundLane::new(1, 1_048_576, 1_048_576);
            let mut grant = None;
            let hello = Hello {
                protocol_min: 1,
                protocol_max: 1,
                client_name: "local-peer-check".into(),
                client_version: "synthetic".into(),
                client_instance_id: "local-peer".into(),
                client_kind: haider_rpc::ClientKind::Tui,
                capabilities_requested: previous_welcome(&row).capabilities_granted,
                features_requested: if row.activity {
                    BTreeSet::from([FEATURE_SESSION_ACTIVITY_V1.to_owned()])
                } else {
                    BTreeSet::new()
                },
                max_receive_frame: if ample {
                    1_048_576
                } else {
                    u32::try_from(reply.len()).expect("peer limit")
                },
                encodings: row.encoding.iter().cloned().collect(),
            };
            let encoding = negotiate_hello(
                hello,
                &context,
                &drain,
                &lane,
                &mut grant,
                if ample { 1_048_576 } else { reply.len() },
            )
            .expect("previous peer connects");
            assert_eq!(
                encoding,
                Some(if row.encoding.is_some() {
                    WireEncoding::MessagePack
                } else {
                    WireEncoding::Json
                })
            );
            let bytes = outbound_bytes(lane.recv().await.expect("Welcome queued").bytes);
            let grant = grant.expect("connection grant");
            if ample {
                assert!(grant.features.contains(FEATURE_SSH_TYPED_SETUP_ERRORS_V1));
            } else {
                assert_eq!(bytes, pinned_welcome_bytes(&reply));
                assert_eq!(grant.features, previous_welcome(&row).features);
            }
        }
    }
}

fn identity_gate_metrics() -> haider_protocol::agent::AgentMetricsSnapshot {
    use haider_protocol::activity::{ActivityKind, ActivityStateV1, ActivityTouchV1};
    haider_protocol::agent::AgentMetricsSnapshot {
        agent: Some(haider_protocol::ids::AgentId::new("identity-gate-agent")),
        session_id: haider_protocol::ids::SessionId::new("identity-gate-child"),
        head_seq: 9,
        started_at_ms: 1,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 2,
        usage: None,
        activity: Some(ActivityStateV1 {
            open: vec![ActivityKind::Edit],
            touched: vec![ActivityTouchV1 {
                kind: ActivityKind::Edit,
                at_ms: 1,
            }],
            latest: Some(ActivityTouchV1 {
                kind: ActivityKind::Edit,
                at_ms: 1,
            }),
            terminal: false,
        }),
    }
}

fn assert_unadvertised_fleet_identity_is_byte_identical() {
    use haider_protocol::ids::{AgentId, RunId, SessionId};

    let root_session_id = SessionId::new("identity-gate-root");
    let child_session_id = SessionId::new("identity-gate-child");
    let agent_id = AgentId::new("identity-gate-agent");
    let fleet = WireFrame::Response {
        request_id: RequestId::new("identity-gate-fleet"),
        body: ResponseBody::SessionFleet {
            snapshot: haider_rpc::SessionFleetSnapshot {
                session_id: root_session_id.clone(),
                generated_at_ms: 1,
                node_limit: 512,
                depth_limit: 32,
                roots: vec![haider_rpc::FleetNodeWire {
                    agent_id: agent_id.clone(),
                    session_id: child_session_id.clone(),
                    callsign: Some("jade-fox-a1b2c3".into()),
                    model: Some("gpt-5.6".into()),
                    provider: Some("openai".into()),
                    task: "inspect identity".into(),
                    depth: 1,
                    parent_session_id: root_session_id.clone(),
                    parent_agent_id: None,
                    state: haider_rpc::FleetAgentStateWire::Live,
                    // 973-activity-icons: populated metrics carry the
                    // activity state the legacy sink must drop too.
                    metrics: Some(identity_gate_metrics()),
                    // 973-activity-icons: the legacy sink omits these too.
                    activity: Some(haider_protocol::activity::ActivityKind::Edit),
                    activities: vec![
                        haider_protocol::activity::ActivityKind::Edit,
                        haider_protocol::activity::ActivityKind::Read,
                    ],
                    folded_children: 0,
                    children: Vec::new(),
                }],
                rollup: haider_rpc::FleetRollupWire {
                    node_count: 1,
                    states: haider_rpc::FleetStateCountsWire {
                        live: 1,
                        ..haider_rpc::FleetStateCountsWire::default()
                    },
                    max_depth: 1,
                    metrics: haider_rpc::FleetMetricsTotalsWire::default(),
                    metrics_complete: false,
                    complete: true,
                },
                truncated: false,
            },
        },
    };
    let descendant_node = haider_rpc::DescendantStreamNodeWire {
        session_id: child_session_id,
        agent_id,
        child_run_id: RunId::new("identity-gate-child-run"),
        parent_session_id: root_session_id.clone(),
        parent_run_id: RunId::new("identity-gate-parent-run"),
        parent_branch_id: None,
        parent_agent_id: None,
        depth: 1,
        callsign: Some("jade-fox-a1b2c3".into()),
        model: Some("gpt-5.6".into()),
        provider: Some("openai".into()),
        task: "inspect identity".into(),
        state: haider_rpc::FleetAgentStateWire::Live,
        requested_after_seq: 0,
        replay_through_seq: 1,
        parent_anchors: haider_rpc::DescendantParentAnchorsWire::default(),
        children: Vec::new(),
    };
    let mut fleet = fleet;
    if let WireFrame::Response {
        body: ResponseBody::SessionFleet { snapshot },
        ..
    } = &mut fleet
    {
        let mut grandchild = snapshot.roots[0].clone();
        grandchild.depth = 2;
        snapshot.roots[0].children.push(grandchild);
    }
    let mut descendant_node = descendant_node;
    let mut grandchild = descendant_node.clone();
    grandchild.depth = 2;
    descendant_node.children.push(grandchild);
    let baseline = WireFrame::Response {
        request_id: RequestId::new("identity-gate-descendants"),
        body: ResponseBody::SessionDescendantsAttach {
            attachment_id: AttachmentId::new("identity-gate-attachment"),
            baseline: haider_rpc::SessionDescendantBaselineWire {
                session_id: root_session_id,
                generated_at_ms: 1,
                fanout: haider_rpc::DescendantFanoutWire {
                    requested_children: 1,
                    accepted_children: 1,
                    hard_limit: haider_rpc::DESCENDANT_STREAM_MAX_CHILDREN,
                },
                truncation: haider_rpc::DescendantTruncationWire {
                    truncated: false,
                    streamed_children: 1,
                    omitted_children: 0,
                    count_complete: true,
                },
                roots: vec![descendant_node.clone()],
            },
        },
    };
    let delta = WireFrame::SessionDescendantStream {
        attachment_id: AttachmentId::new("identity-gate-attachment"),
        event: haider_rpc::SessionDescendantStreamEventWire::Delta {
            change: haider_rpc::DescendantChangeKindWire::Updated,
            child: descendant_node,
        },
    };

    let roster: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "identity-gate-child",
        "head_seq": 9,
        "worker_generation": 1,
    }))
    .expect("minimal summary");
    let roster = WireFrame::SessionRosterDelta {
        summaries: vec![haider_rpc::SessionSummary {
            owner_agent_id: None,
            launch_origin: None,
            agent_metrics: Some(identity_gate_metrics()),
            ..roster
        }],
    };
    let summary: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "identity-gate-child",
        "head_seq": 9,
        "worker_generation": 1,
    }))
    .expect("minimal list summary");
    let summary = haider_rpc::SessionSummary {
        owner_agent_id: None,
        launch_origin: None,
        agent_metrics: Some(identity_gate_metrics()),
        ..summary
    };
    let digest: haider_rpc::SessionObserveDigest = serde_json::from_value(serde_json::json!({
        "session_id": "identity-gate-child",
        "head_seq": 9,
        "worker_generation": 1,
        "title": "identity gate",
        "run_state": "running",
        "main_head_seq": 9,
        "updated_at_ms": 1,
    }))
    .expect("minimal observe digest");
    let digest = haider_rpc::SessionObserveDigest {
        agent_metrics: Some(identity_gate_metrics()),
        ..digest
    };
    let list = WireFrame::Response {
        request_id: RequestId::new("identity-gate-list"),
        body: ResponseBody::SessionList {
            sessions: vec![summary],
            next_cursor: None,
        },
    };
    let observe = WireFrame::Response {
        request_id: RequestId::new("identity-gate-observe"),
        body: ResponseBody::SessionObserve {
            digest: digest.clone(),
        },
    };
    let observe_batch = WireFrame::Response {
        request_id: RequestId::new("identity-gate-observe-batch"),
        body: ResponseBody::SessionObserveBatch {
            digests: vec![digest],
        },
    };
    let rich_frames = [fleet, baseline, delta, roster, list, observe, observe_batch];
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        let sink = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: false,
            activity: false,
        };
        for rich_frame in &rich_frames {
            let mut legacy_frame = (*rich_frame).clone();
            match &mut legacy_frame {
                WireFrame::Response {
                    body: ResponseBody::SessionFleet { snapshot },
                    ..
                } => {
                    for node in &mut snapshot.roots {
                        node.model = None;
                        node.provider = None;
                        node.activity = None;
                        node.activities.clear();
                        if let Some(metrics) = node.metrics.as_mut() {
                            metrics.activity = None;
                        }
                        let child = &mut node.children[0];
                        child.model = None;
                        child.provider = None;
                        child.activity = None;
                        child.activities.clear();
                        if let Some(metrics) = child.metrics.as_mut() {
                            metrics.activity = None;
                        }
                    }
                }
                WireFrame::SessionRosterDelta { summaries } => {
                    if let Some(metrics) = summaries[0].agent_metrics.as_mut() {
                        metrics.activity = None;
                    }
                }
                WireFrame::Response {
                    body: ResponseBody::SessionList { sessions, .. },
                    ..
                } => {
                    if let Some(metrics) = sessions[0].agent_metrics.as_mut() {
                        metrics.activity = None;
                    }
                }
                WireFrame::Response {
                    body: ResponseBody::SessionObserve { digest },
                    ..
                } => {
                    if let Some(metrics) = digest.agent_metrics.as_mut() {
                        metrics.activity = None;
                    }
                }
                WireFrame::Response {
                    body: ResponseBody::SessionObserveBatch { digests },
                    ..
                } => {
                    if let Some(metrics) = digests[0].agent_metrics.as_mut() {
                        metrics.activity = None;
                    }
                }
                WireFrame::Response {
                    body: ResponseBody::SessionDescendantsAttach { baseline, .. },
                    ..
                } => {
                    baseline.roots[0].model = None;
                    baseline.roots[0].provider = None;
                    baseline.roots[0].children[0].model = None;
                    baseline.roots[0].children[0].provider = None;
                }
                WireFrame::SessionDescendantStream {
                    event: haider_rpc::SessionDescendantStreamEventWire::Delta { child, .. },
                    ..
                } => {
                    child.model = None;
                    child.provider = None;
                    child.children[0].model = None;
                    child.children[0].provider = None;
                }
                _ => unreachable!("identity gate fixture covers all delivery shapes"),
            }
            if matches!(
                legacy_frame,
                WireFrame::Response {
                    body: ResponseBody::SessionFleet { .. }
                        | ResponseBody::SessionDescendantsAttach { .. },
                    ..
                } | WireFrame::SessionDescendantStream {
                    event: haider_rpc::SessionDescendantStreamEventWire::Delta { .. },
                    ..
                }
            ) {
                assert!(
                    serde_json::to_string(&legacy_frame)
                        .expect("legacy identity frame serializes")
                        .contains("\"callsign\":\"jade-fox-a1b2c3\""),
                    "the pre-existing callsign field survives the X1 feature gate"
                );
            }
            assert!(
                !serde_json::to_string(&legacy_frame)
                    .expect("legacy frame serializes")
                    .contains("\"activit"),
                "no 973 activity field reaches a legacy peer"
            );
            let expected = uds_codec::encode_with(&legacy_frame, 1_048_576, encoding)
                .expect("legacy identity frame encodes");
            assert!(
                !expected.windows(7).any(|part| part == b"activit"),
                "legacy {encoding:?} frame must not encode activity keys"
            );
            let encoded = sink.encode(rich_frame).expect("legacy sink encodes");
            let actual = match encoded {
                OutboundBytes::Contiguous(bytes) => bytes.as_slice().to_vec(),
                OutboundBytes::Framed(frame) => {
                    let mut bytes = Vec::with_capacity(frame.framed_len());
                    bytes.extend_from_slice(frame.prefix());
                    bytes.extend_from_slice(frame.body());
                    bytes
                }
                OutboundBytes::Event(frame) => {
                    let mut bytes = Vec::with_capacity(frame.framed_len());
                    frame
                        .write_to(&mut bytes)
                        .expect("event frame reconstructs");
                    bytes
                }
            };
            assert_eq!(actual, expected);
            let inconsistent = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: false,
                activity: true,
            };
            assert_eq!(
                outbound_bytes(
                    inconsistent
                        .encode(rich_frame)
                        .expect("class 1 remains legacy")
                ),
                expected
            );
            // Released 966–972 peers negotiate fleet identity but never
            // request session_activity_v1. They retain model/provider while
            // every activity key is absent in both encodings.
            let mut identity_only = legacy_frame.clone();
            match &mut identity_only {
                WireFrame::Response {
                    body: ResponseBody::SessionFleet { snapshot },
                    ..
                } => {
                    snapshot.roots[0].model = Some("gpt-5.6".into());
                    snapshot.roots[0].provider = Some("openai".into());
                    snapshot.roots[0].children[0].model = Some("gpt-5.6".into());
                    snapshot.roots[0].children[0].provider = Some("openai".into());
                }
                WireFrame::Response {
                    body: ResponseBody::SessionDescendantsAttach { baseline, .. },
                    ..
                } => {
                    baseline.roots[0].model = Some("gpt-5.6".into());
                    baseline.roots[0].provider = Some("openai".into());
                    baseline.roots[0].children[0].model = Some("gpt-5.6".into());
                    baseline.roots[0].children[0].provider = Some("openai".into());
                }
                WireFrame::SessionDescendantStream {
                    event: haider_rpc::SessionDescendantStreamEventWire::Delta { child, .. },
                    ..
                } => {
                    child.model = Some("gpt-5.6".into());
                    child.provider = Some("openai".into());
                    child.children[0].model = Some("gpt-5.6".into());
                    child.children[0].provider = Some("openai".into());
                }
                _ => {}
            }
            let class2 = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: true,
                activity: false,
            };
            let expected_class2 = uds_codec::encode_with(&identity_only, 1_048_576, encoding)
                .expect("identity-only frame encodes");
            assert_eq!(
                outbound_bytes(class2.encode(rich_frame).expect("class 2 encodes")),
                expected_class2
            );
            assert!(
                !serde_json::to_string(&identity_only)
                    .expect("class 2 JSON")
                    .contains("\"activit")
            );
            if matches!(
                rich_frame,
                WireFrame::Response {
                    body: ResponseBody::SessionFleet { .. },
                    ..
                }
            ) {
                assert!(
                    serde_json::to_string(&identity_only)
                        .expect("fleet JSON")
                        .contains("\"model\":\"gpt-5.6\"")
                );
            }
            let class3 = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: true,
                activity: true,
            };
            assert_eq!(
                outbound_bytes(class3.encode(rich_frame).expect("class 3 encodes")),
                uds_codec::encode_with(rich_frame, 1_048_576, encoding)
                    .expect("full frame encodes")
            );
        }
    }
}

fn outbound_bytes(encoded: OutboundBytes) -> Vec<u8> {
    match encoded {
        OutboundBytes::Contiguous(bytes) => bytes.as_slice().to_vec(),
        OutboundBytes::Framed(frame) => {
            let mut bytes = Vec::with_capacity(frame.framed_len());
            bytes.extend_from_slice(frame.prefix());
            bytes.extend_from_slice(frame.body());
            bytes
        }
        OutboundBytes::Event(frame) => {
            let mut bytes = Vec::with_capacity(frame.framed_len());
            frame
                .write_to(&mut bytes)
                .expect("event frame reconstructs");
            bytes
        }
    }
}

#[tokio::test]
async fn standard_welcome_cache_reuses_byte_identical_golden_frame() {
    let (_root, hub) = liveness_hub().await;
    let context = liveness_context(hub);
    let frame_limit = u32::try_from(context.frame_limit).expect("test frame limit");
    let first = cached_standard_welcome(
        &context,
        haider_rpc::WIRE_PROTOCOL_VERSION,
        frame_limit,
        context.frame_limit,
    )
    .expect("first cached Welcome");
    let second = cached_standard_welcome(
        &context,
        haider_rpc::WIRE_PROTOCOL_VERSION,
        frame_limit,
        context.frame_limit,
    )
    .expect("second cached Welcome");
    let expected = uds_codec::encode(
        &WireFrame::Welcome(Welcome {
            protocol: haider_rpc::WIRE_PROTOCOL_VERSION,
            instance_id: context.instance_id.clone(),
            daemon_generation: context.daemon_generation,
            frame_limit,
            profile_id: context.profile_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            lifecycle_phase: LifecyclePhase::Ready,
            capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
            features: welcome_features(),
            user_command_withheld: false,
            encoding: None,
        }),
        context.frame_limit,
    )
    .expect("golden Welcome frame");

    assert_eq!(first.features, second.features);
    let (OutboundBytes::Contiguous(first), OutboundBytes::Contiguous(second)) =
        (&first.bytes, &second.bytes)
    else {
        panic!("standard JSON Welcome must use contiguous cached bytes");
    };
    assert!(Arc::ptr_eq(first, second));
    assert_eq!(first.as_slice(), expected.as_slice());
    assert_eq!(second.as_slice(), expected.as_slice());
}

/// MUTATION CHECK: replace the round-robin ring with one FIFO/hot-lane drain.
/// Expected failure: `a-2` is returned before the waiting `b-1`.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn fair_outbox_visits_a_cold_attachment_before_returning_to_the_hot_one() {
    let lane = OutboundLane::new(8, 1_024, 64);
    let hot = LaneKey::Attachment(AttachmentId::new("hot"));
    let cold = LaneKey::Attachment(AttachmentId::new("cold"));
    lane.try_push(hot.clone(), ordinary(b"a-1")).expect("hot 1");
    lane.try_push(hot.clone(), ordinary(b"a-2")).expect("hot 2");
    lane.try_push(hot, ordinary(b"a-3")).expect("hot 3");
    lane.try_push(cold, ordinary(b"b-1")).expect("cold");

    assert_eq!(lane.recv().await.expect("first").bytes, b"a-1");
    assert_eq!(lane.recv().await.expect("second").bytes, b"b-1");
}

/// MUTATION CHECK: restore W3b1's "notice is last" writer ordering. Expected
/// failure: checkpoint bytes precede `ServerDraining` bytes instead of
/// following the notice at the next complete-frame boundary.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn drain_notice_precedes_queued_checkpoint_traffic_and_keeps_one_deadline() {
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let (_reader, writer) = server.into_split();
    let lane = OutboundLane::new(4, 1_024, 64);
    lane.try_push(LaneKey::System, ordinary(b"checkpoint"))
        .expect("checkpoint queues");
    lane.close();
    let (reserve, reserved) = mpsc::channel(1);
    reserve
        .try_send(ReservedNotice {
            bytes: b"notice".to_vec(),
            deadline: Instant::now() + std::time::Duration::from_secs(5),
        })
        .expect("notice reserves");
    drop(reserve);

    assert!(run_writer(writer, lane, reserved).await.expect("writer"));
    let mut received = Vec::new();
    client
        .read_to_end(&mut received)
        .await
        .expect("read to EOF");
    assert_eq!(received, b"noticecheckpoint");
}

/// MUTATION CHECK: purge a detached lane without refunding its bytes/frames.
/// Expected failure: the replacement frame is refused by the stale charge.
#[tokio::test]
async fn detach_purge_refunds_the_bounded_outbox_budget() {
    let lane = OutboundLane::new(2, 8, 64);
    let attachment = AttachmentId::new("attachment");
    lane.try_push(
        LaneKey::Attachment(attachment.clone()),
        ordinary(b"12345678"),
    )
    .expect("full budget queues");
    let _ = lane.purge(&attachment);
    lane.try_push(LaneKey::System, ordinary(b"replaced"))
        .expect("purge refunded bytes and frame");
}

/// MUTATION CHECK: route `Lagged` back through its attachment's lane in
/// `attachment_lane`. Expected failure: every detach cycle recreates an
/// ownerless lane and the zero-residual assertion below fails.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn repeated_detach_cycles_leave_zero_residual_attachment_lanes() {
    let lane = OutboundLane::new(8, 64 * 1_024, 512);
    for cycle in 0..100 {
        let attachment = AttachmentId::new(format!("attachment-{cycle}"));
        lane.try_push(
            LaneKey::System,
            staged_response(&attachment, &format!("request-{cycle}"), b"attach-resp"),
        )
        .expect("response stages");
        // Response pops (client received it), one event flows, the client
        // stalls, and the attachment is purged.
        assert_eq!(
            lane.recv().await.expect("response pops").bytes,
            b"attach-resp"
        );
        assert!(matches!(
            lane.offer(
                LaneKey::Attachment(attachment.clone()),
                b"event".to_vec(),
                None
            ),
            SendAdmission::Sent
        ));
        let _ = lane.purge(&attachment);
        // The Lagged control notice is exactly what `lag_and_detach` sends:
        // system-lane traffic, keyed to nothing.
        let lagged = attachment_lane(&WireFrame::Lagged {
            attachment_id: attachment.clone(),
            last_queued_seq: 1,
        });
        assert_eq!(lagged, LaneKey::System, "Lagged is a control notice");
        lane.try_push(lagged, ordinary(b"lagged"))
            .expect("lagged queues");
        assert_eq!(
            lane.attachment_lane_count(),
            0,
            "purged attachment lane must never be recreated"
        );
        // Drain the cycle's system traffic so budgets stay clean.
        let frame = lane.recv().await.expect("lagged pops");
        lane.credit(&frame);
    }
    assert_eq!(lane.attachment_lane_count(), 0);
}

/// MUTATION CHECK: drop the pending-response gate from `offer` (admit events
/// while the staged attach response is still queued). Expected failure: the
/// event is admitted early and pops before the response that names its
/// attachment id.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn event_offers_wait_for_the_staged_attach_response_to_pop() {
    let lane = OutboundLane::new(8, 1_024, 64);
    let attachment = AttachmentId::new("gated");
    lane.try_push(LaneKey::System, ordinary(b"list-resp"))
        .expect("earlier reply queues");
    lane.try_push(
        LaneKey::System,
        staged_response(&attachment, "attach", b"attach-resp"),
    )
    .expect("response stages");
    assert!(matches!(
        lane.offer(
            LaneKey::Attachment(attachment.clone()),
            b"event-1".to_vec(),
            None
        ),
        SendAdmission::Busy
    ));
    assert_eq!(lane.recv().await.expect("first").bytes, b"list-resp");
    assert!(matches!(
        lane.offer(
            LaneKey::Attachment(attachment.clone()),
            b"event-1".to_vec(),
            None
        ),
        SendAdmission::Busy
    ));
    assert_eq!(lane.recv().await.expect("second").bytes, b"attach-resp");
    assert!(matches!(
        lane.offer(LaneKey::Attachment(attachment), b"event-1".to_vec(), None),
        SendAdmission::Sent
    ));
    assert_eq!(lane.recv().await.expect("third").bytes, b"event-1");
}

/// MUTATION CHECK: remove the reply floor's fall-through in `try_push`, or
/// stop serving the floor before the fair ring. Expected failure: the reply
/// pushed against camped ordinary capacity is refused, or it fails to pop
/// ahead of the camped event frames.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn reply_floor_admits_and_pops_ahead_of_camped_event_traffic() {
    let lane = OutboundLane::new(2, 1_024, 64);
    let camped = LaneKey::Attachment(AttachmentId::new("camped"));
    assert!(matches!(
        lane.offer(camped.clone(), b"event-1".to_vec(), None),
        SendAdmission::Sent
    ));
    lane.try_push(LaneKey::System, ordinary(b"reply-1"))
        .expect("ordinary reply fits");
    // Aggregate cap (2) is now camped: this reply can only enter the floor.
    lane.try_push(LaneKey::System, ordinary(b"reply-2"))
        .expect("floor admits the reply");
    // The floor frame pops FIRST, ahead of everything in the fair ring.
    assert_eq!(lane.recv().await.expect("floor first").bytes, b"reply-2");
    // A second floor push while the floor is in use is refused terminally.
    lane.try_push(LaneKey::System, ordinary(b"reply-3"))
        .expect_err("floor in use and ordinary camped");
}

/// Monitor replay records use one paced ordered lane, never the reply floor.
/// MUTATION CHECK: route an ordered record through `try_push`/System or let it
/// borrow the floor. Expected failure: caught-up may pop before the earlier
/// report, allowing a client to persist a cursor that skipped it.
#[tokio::test]
async fn ordered_stream_preserves_response_report_and_caught_up_order() {
    let lane = OutboundLane::new(2, 1_024, 64);
    let ordered = LaneKey::Ordered("monitor-watch-order".into());
    lane.try_push(LaneKey::System, ordinary(b"watch-response"))
        .expect("watch response enters system lane");
    assert_eq!(
        lane.offer(ordered.clone(), b"monitor-report".to_vec(), None),
        SendAdmission::Sent
    );
    assert_eq!(
        lane.offer(ordered.clone(), b"monitor-caught-up".to_vec(), None),
        SendAdmission::Busy,
        "ordered traffic waits instead of overtaking through the reply floor"
    );
    assert_eq!(lane.inner.state.lock().expect("state lock").floor_in_use, 0);

    let response = lane.recv().await.expect("response first");
    assert_eq!(response.bytes, b"watch-response");
    lane.credit(&response);
    assert_eq!(
        lane.offer(ordered.clone(), b"monitor-caught-up".to_vec(), None),
        SendAdmission::Busy,
        "the queued report still occupies the ordered lane's sole slot"
    );
    let report = lane.recv().await.expect("report second");
    assert_eq!(report.bytes, b"monitor-report");
    assert_eq!(
        lane.offer(ordered, b"monitor-caught-up".to_vec(), None),
        SendAdmission::Sent,
        "popping the report releases the ordered lane before byte credit"
    );
    lane.credit(&report);
    let caught_up = lane.recv().await.expect("caught-up last");
    assert_eq!(caught_up.bytes, b"monitor-caught-up");
    lane.credit(&caught_up);
}

/// Roster deltas are best-effort observations, never replies. When ordinary
/// capacity is camped they must leave the sole reply floor untouched.
#[tokio::test]
async fn droppable_roster_refusal_preserves_the_reply_floor() {
    let lane = OutboundLane::new(1, 1_024, 64);
    assert!(matches!(
        lane.offer(
            LaneKey::Attachment(AttachmentId::new("camped")),
            b"event".to_vec(),
            None
        ),
        SendAdmission::Sent
    ));

    lane.try_push_droppable(LaneKey::System, ordinary(b"roster"))
        .expect_err("roster cannot borrow the reply floor");
    assert_eq!(lane.inner.state.lock().expect("state lock").floor_in_use, 0);

    lane.try_push(LaneKey::System, ordinary(b"reply"))
        .expect("reply still enters its reserved floor");
    assert_eq!(lane.recv().await.expect("floor reply").bytes, b"reply");
}

/// MUTATION CHECK: stop firing admission tickets in arrival order (fire the
/// newest instead). Expected failure: the head waiter's ticket never fires
/// while later tickets do, and the first receiver below times out.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn admission_tickets_fire_in_arrival_order_as_capacity_frees() {
    let lane = OutboundLane::new(1, 1_024, 64);
    let camped = LaneKey::Attachment(AttachmentId::new("camped"));
    assert!(matches!(
        lane.offer(camped, b"camping".to_vec(), None),
        SendAdmission::Sent
    ));
    let first = lane.drain_ticket();
    let second = lane.drain_ticket();
    // Pop frees one frame slot: exactly the FIRST ticket fires.
    let camped_frame = lane.recv().await.expect("camped frame pops");
    tokio::time::timeout(std::time::Duration::from_secs(5), first.notified())
        .await
        .expect("head ticket fires on the pop, in order");
    assert!(matches!(
        lane.offer(
            LaneKey::Attachment(AttachmentId::new("first")),
            b"first".to_vec(),
            Some(&first)
        ),
        SendAdmission::Sent
    ));
    // The first holder's admission consumed only its own reservation. Its
    // pop frees the next service turn and fires the second token.
    let first_frame = lane.recv().await.expect("first waiter frame pops");
    tokio::time::timeout(std::time::Duration::from_secs(5), second.notified())
        .await
        .expect("second ticket fires after first service");
    assert!(matches!(
        lane.offer(
            LaneKey::Attachment(AttachmentId::new("second")),
            b"second".to_vec(),
            Some(&second)
        ),
        SendAdmission::Sent
    ));
    lane.credit(&camped_frame);
    lane.credit(&first_frame);
}

/// The reviewer's exact barging schedule: capacity one, the head ticket fires,
/// then a fresh hot offer arrives before the head task can re-offer.
///
/// MUTATION CHECK: remove the `(tickets empty || presented token is head)`
/// admission gate in `OutboundLane::offer`. Expected failure: `hot` is
/// admitted in the fired-head window instead of returning `Busy`.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn a_fresh_offer_cannot_barge_after_the_head_ticket_fires() {
    let lane = OutboundLane::new(1, 1_024, 64);
    let camped = LaneKey::Attachment(AttachmentId::new("camped"));
    let cold = LaneKey::Attachment(AttachmentId::new("cold"));
    let hot = LaneKey::Attachment(AttachmentId::new("hot"));
    assert!(matches!(
        lane.offer(camped, b"camping".to_vec(), None),
        SendAdmission::Sent
    ));
    assert!(matches!(
        lane.offer(cold.clone(), b"cold".to_vec(), None),
        SendAdmission::Busy
    ));
    let head = lane.drain_ticket();
    assert!(matches!(
        lane.offer(cold.clone(), b"cold".to_vec(), Some(&head)),
        SendAdmission::Busy
    ));

    let camped_frame = lane.recv().await.expect("capacity frees");
    head.notified().await;
    assert!(
        matches!(lane.offer(hot, b"hot".to_vec(), None), SendAdmission::Busy),
        "a fresh hot offer must park behind the fired head"
    );
    assert!(matches!(
        lane.offer(cold, b"cold".to_vec(), Some(&head)),
        SendAdmission::Sent
    ));
    assert_eq!(lane.recv().await.expect("head is served").bytes, b"cold");
    lane.credit(&camped_frame);

    // Exercise the fallback independently: even if a ticket loses its final
    // strong reference without the guard, a fresh offer that prunes that dead
    // head must fire the live successor while the frame slot is open.
    let fallback = OutboundLane::new(1, 1_024, 64);
    assert!(matches!(
        fallback.offer(
            LaneKey::Attachment(AttachmentId::new("fallback-camped")),
            b"fallback-camped".to_vec(),
            None
        ),
        SendAdmission::Sent
    ));
    let dead_head = fallback.drain_ticket();
    let live_successor = fallback.drain_ticket();
    let fallback_frame = fallback.recv().await.expect("fallback capacity frees");
    dead_head.notified().await;
    drop(dead_head);
    assert!(matches!(
        fallback.offer(
            LaneKey::Attachment(AttachmentId::new("fallback-fresh")),
            b"fallback-fresh".to_vec(),
            None
        ),
        SendAdmission::Busy
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), live_successor.notified())
        .await
        .expect("dead-head pruning fires the live successor");
    fallback.credit(&fallback_frame);

    // The same defense applies when cancellation, rather than a fresh offer,
    // is the operation that discovers the dead head.
    let cancellation_fallback = OutboundLane::new(1, 1_024, 64);
    assert!(matches!(
        cancellation_fallback.offer(
            LaneKey::Attachment(AttachmentId::new("cancel-camped")),
            b"cancel-camped".to_vec(),
            None
        ),
        SendAdmission::Sent
    ));
    let dead_head = cancellation_fallback.drain_ticket();
    let live_successor = cancellation_fallback.drain_ticket();
    let cancelled_tail = cancellation_fallback.drain_ticket();
    let cancellation_frame = cancellation_fallback
        .recv()
        .await
        .expect("cancellation fallback capacity frees");
    dead_head.notified().await;
    drop(dead_head);
    cancellation_fallback.cancel_ticket(&cancelled_tail);
    tokio::time::timeout(std::time::Duration::from_secs(5), live_successor.notified())
        .await
        .expect("cancellation-side pruning fires the live successor");
    cancellation_fallback.credit(&cancellation_frame);
}

/// Persistent fresh hot traffic cannot consume even one service turn ahead
/// of an already-queued cold waiter.
///
/// MUTATION CHECK: let fresh offers ignore a non-empty ticket queue.
/// Expected failure: one of the 64 hot offers is admitted before `cold`.
#[tokio::test]
async fn a_persistent_hot_offerer_cannot_starve_the_cold_head_waiter() {
    let lane = OutboundLane::new(1, 4_096, 64);
    let camped = LaneKey::Attachment(AttachmentId::new("camped"));
    let cold = LaneKey::Attachment(AttachmentId::new("cold"));
    let hot = LaneKey::Attachment(AttachmentId::new("hot"));
    assert!(matches!(
        lane.offer(camped, b"camping".to_vec(), None),
        SendAdmission::Sent
    ));
    assert!(matches!(
        lane.offer(cold.clone(), b"cold".to_vec(), None),
        SendAdmission::Busy
    ));
    let head = lane.drain_ticket();
    let camped_frame = lane.recv().await.expect("capacity frees");
    head.notified().await;

    for turn in 0..64 {
        assert!(
            matches!(
                lane.offer(hot.clone(), format!("hot-{turn}").into_bytes(), None),
                SendAdmission::Busy
            ),
            "hot offer barged on bounded turn {turn}"
        );
    }
    assert!(matches!(
        lane.offer(cold, b"cold".to_vec(), Some(&head)),
        SendAdmission::Sent
    ));
    assert_eq!(lane.recv().await.expect("cold is served").bytes, b"cold");
    lane.credit(&camped_frame);
}

/// MUTATION CHECK: validate the byte budget against the frame body alone
/// again. Expected failure: `L + 3` is accepted even though the maximum Event
/// below occupies `L + 4` encoded bytes and can never enter an empty outbox.
#[test]
fn queued_byte_budget_covers_the_four_byte_prefix_at_the_exact_boundary() {
    let session_id = haider_protocol::ids::SessionId::new("budget-boundary");
    let attachment_id = AttachmentId::new("budget-boundary-attachment");
    let event = WireFrame::Event {
        attachment_id: attachment_id.clone(),
        session_id: session_id.clone(),
        envelope: haider_protocol::envelope::EventEnvelope {
            schema_version: haider_protocol::envelope::SCHEMA_VERSION,
            event_id: haider_protocol::ids::EventId::new("budget-boundary-event"),
            seq: 1,
            session_id,
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: haider_protocol::ids::DeviceId::new("budget-boundary-device"),
            authority_epoch: 1,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 1,
            render: haider_protocol::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: haider_protocol::envelope::PromptRender::Omit,
            },
            payload: serde_json::json!({"type": "budget_boundary"}).into(),
        },
    };
    let encoded =
        uds_codec::encode(&event, haider_rpc::DEFAULT_FRAME_LIMIT).expect("probe Event encodes");
    let frame_limit = encoded.len().saturating_sub(4);
    assert_eq!(encoded.len(), frame_limit + 4);

    let mut config = crate::DaemonConfig::new(
        "budget-boundary",
        "/tmp/unused-store",
        "/tmp/unused-runtime",
    );
    config.frame_limit = frame_limit;
    config.outbound_queued_bytes = frame_limit + 3;
    let rejected = config.validate().expect_err("L + 3 must be rejected");
    assert!(rejected.contains("4-byte UDS length prefix"));

    config.outbound_queued_bytes = frame_limit + 4;
    config.validate().expect("L + 4 is the exact minimum");
    let sink = ConnectionFrameSink {
        lane: OutboundLane::new(1, config.outbound_queued_bytes, frame_limit + 4),
        outbound_limit: frame_limit,
        encoding: haider_rpc::WireEncoding::Json,
        fleet_identity: true,
        activity: true,
    };
    assert!(
        matches!(sink.offer(&attachment_id, &event), SendAdmission::Sent),
        "a maximum-size legal Event must enter an empty ordinary outbox"
    );
}

// ───────────────────────── R9 dead-peer policy ──────────────────────────────

// MUTATION CHECK (R9 verdict arithmetic): weaken either deadline comparison
// (`>=` -> `>` at the exact boundary, or drop the backlog gate). Expected
// failure: the exact-boundary assertions below.
#[tokio::test(start_paused = true)]
async fn liveness_verdict_pins_both_deadlines_exactly() {
    let start = Instant::now();
    tokio::time::advance(READ_IDLE_DEADLINE - std::time::Duration::from_millis(1)).await;
    let now = Instant::now();
    // One millisecond before the read deadline: alive.
    assert_eq!(liveness_breach(now, start, now, false), None);
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    let now = Instant::now();
    // Exactly at the read deadline: closed.
    assert_eq!(
        liveness_breach(now, start, now, false),
        Some("idle_timeout")
    );

    // Write progress: stalled backlog closes at exactly 45s WITHOUT read
    // idleness; no backlog never trips the write deadline.
    let progress_start = Instant::now();
    tokio::time::advance(WRITE_PROGRESS_DEADLINE).await;
    let now = Instant::now();
    assert_eq!(
        liveness_breach(now, now, progress_start, true),
        Some("write_stalled")
    );
    assert_eq!(liveness_breach(now, now, progress_start, false), None);
}

fn liveness_context(hub: crate::session_hub::SessionHub) -> ConnectionContext {
    let (writers, receiver) = mpsc::unbounded_channel();
    // Leak the receiver so writer registration succeeds; the test joins the
    // connection task itself.
    std::mem::forget(receiver);
    connection_context(hub, writers)
}

fn connection_context(
    hub: crate::session_hub::SessionHub,
    writers: WriterRegistry,
) -> ConnectionContext {
    ConnectionContext {
        profile_id: "profile-liveness".into(),
        instance_id: "instance-liveness".into(),
        daemon_generation: 1,
        frame_limit: 1024 * 1024,
        outbound_queue_capacity: 8,
        outbound_queued_bytes: 4 * 1024 * 1024,
        max_connections: 4,
        handshake_timeout: std::time::Duration::from_secs(10),
        writers,
        owner_uid: rustix::process::geteuid().as_raw(),
        hub,
        shutdown: crate::lifecycle::ShutdownHandle::channel().0,
        endpoint_path: std::path::PathBuf::from("/tmp/liveness-test.sock"),
        pid_file_path: std::path::PathBuf::from("/tmp/haiderd.pid"),
        idle_ttl_ms: None,
        warm: false,
        readiness: crate::lifecycle::ready_for_tests(),
    }
}

#[tokio::test]
async fn kernel_reported_different_uid_peer_is_rejected_before_framing() {
    let (_dir, hub) = liveness_hub().await;
    let (client, server) = UnixStream::pair().expect("real UDS pair");
    let actual_uid = server.peer_cred().expect("kernel credentials").uid();
    let mut context = liveness_context(hub.clone());
    context.owner_uid = actual_uid.checked_add(1).expect("different test uid");
    let (_drain, drain) = watch::channel(None);
    let error = serve(server, context, drain)
        .await
        .expect_err("different UID must be rejected");
    assert!(
        error
            .to_string()
            .contains(&format!("refusing peer uid {actual_uid}"))
    );
    drop(client);
    hub.shutdown().await.expect("hub shutdown");
}

async fn handshake_over(client: &mut UnixStream) {
    use tokio::io::AsyncWriteExt;
    let hello = haider_rpc::WireFrame::Hello(haider_rpc::Hello {
        protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
        protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
        client_name: "liveness-test".into(),
        client_version: "test".into(),
        client_instance_id: "client-liveness".into(),
        client_kind: haider_rpc::ClientKind::Cli,
        capabilities_requested: CapabilitySet::from([Capability::View, Capability::Control]),
        features_requested: Default::default(),
        max_receive_frame: 1024 * 1024,
        encodings: Vec::new(),
    });
    let bytes = uds_codec::encode(&hello, 1024 * 1024).expect("hello encodes");
    client.write_all(&bytes).await.expect("hello writes");
    let mut decoder = uds_codec::Decoder::new(1024 * 1024);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = client.read(&mut buffer).await.expect("welcome reads");
        assert_ne!(read, 0, "server closed during handshake");
        let batch = decoder.push(&buffer[..read]);
        if batch
            .frames
            .iter()
            .any(|frame| matches!(frame, haider_rpc::WireFrame::Welcome(_)))
        {
            return;
        }
    }
}

async fn liveness_hub() -> (tempfile::TempDir, crate::session_hub::SessionHub) {
    let dir = tempfile::Builder::new()
        .prefix("hlive")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let store = haider_core::SqliteStoreHandle::open(dir.path())
        .await
        .expect("open store");
    let hub =
        crate::session_hub::SessionHub::new(store, crate::session_hub::SessionHubConfig::default())
            .expect("hub");
    (dir, hub)
}

/// No-bind client for end-to-end connection laws. `UnixStream::pair` keeps
/// the real platform split, framing, bounded writer, hub connection, and
/// liveness loop while avoiding the filesystem listener that sandboxed test
/// environments cannot create.
struct PairedClient {
    stream: UnixStream,
    decoder: uds_codec::Decoder,
    buffered: Vec<u8>,
    encoding: haider_rpc::WireEncoding,
}

struct BindingChannelSink(mpsc::UnboundedSender<WireFrame>);

impl FrameSink for BindingChannelSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.send(frame).map_err(|_| FrameSendError)
    }
}

impl PairedClient {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            decoder: uds_codec::Decoder::new(1024 * 1024),
            buffered: Vec::new(),
            encoding: haider_rpc::WireEncoding::Json,
        }
    }

    async fn send(&mut self, frame: &WireFrame) {
        let bytes = uds_codec::encode_with(frame, 1024 * 1024, self.encoding)
            .expect("paired frame encodes");
        self.stream
            .write_all(&bytes)
            .await
            .expect("paired frame writes");
    }

    async fn next(&mut self) -> Option<WireFrame> {
        loop {
            if !self.buffered.is_empty() {
                let step = self.decoder.push_one(&self.buffered);
                self.buffered.drain(..step.consumed);
                assert!(step.error.is_none(), "server sent an invalid paired frame");
                if let Some(frame) = step.frame {
                    return Some(frame);
                }
            }
            let mut buffer = [0_u8; 4096];
            let read = self.stream.read(&mut buffer).await.expect("paired read");
            if read == 0 {
                return None;
            }
            self.buffered.extend_from_slice(&buffer[..read]);
        }
    }

    async fn handshake(&mut self) {
        self.handshake_with(CapabilitySet::from([Capability::View, Capability::Control]))
            .await;
    }

    async fn handshake_with(&mut self, capabilities: CapabilitySet) {
        self.handshake_named_with("paired-connection-test", capabilities)
            .await;
    }

    async fn handshake_named_with(&mut self, client_name: &str, capabilities: CapabilitySet) {
        self.send(&WireFrame::Hello(haider_rpc::Hello {
            protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
            protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
            client_name: client_name.into(),
            client_version: "test".into(),
            client_instance_id: "paired-client".into(),
            client_kind: haider_rpc::ClientKind::Headless,
            capabilities_requested: capabilities,
            features_requested: Default::default(),
            max_receive_frame: 1024 * 1024,
            encodings: Vec::new(),
        }))
        .await;
        assert!(matches!(self.next().await, Some(WireFrame::Welcome(_))));
        assert!(matches!(
            self.next().await,
            Some(WireFrame::ResidentSessionBinding {
                session_id: None,
                ..
            })
        ));
    }

    async fn handshake_msgpack(&mut self) {
        self.send(&WireFrame::Hello(haider_rpc::Hello {
            protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
            protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
            client_name: "paired-msgpack-test".into(),
            client_version: "test".into(),
            client_instance_id: "paired-msgpack-client".into(),
            client_kind: haider_rpc::ClientKind::Headless,
            capabilities_requested: CapabilitySet::from([Capability::View]),
            features_requested: Default::default(),
            max_receive_frame: 1024 * 1024,
            encodings: vec!["msgpack".into()],
        }))
        .await;
        assert!(matches!(
            self.next().await,
            Some(WireFrame::Welcome(Welcome {
                encoding: Some(ref encoding),
                ..
            })) if encoding == "msgpack"
        ));
        self.encoding = haider_rpc::WireEncoding::MessagePack;
        self.decoder.set_encoding(self.encoding);
        assert!(matches!(
            self.next().await,
            Some(WireFrame::ResidentSessionBinding {
                session_id: None,
                ..
            })
        ));
    }

    async fn request(&mut self, request: &str, body: haider_rpc::RequestBody) {
        self.send(&WireFrame::Request {
            request_id: RequestId::new(request),
            body,
        })
        .await;
    }
}

/// MUTATION CHECK: remove the Control gate or the graceful ShutdownHandle
/// request in `handle_frame`. Expected runtime failure: the view-only peer
/// starts draining, the control peer never advances the lifecycle watch, or a
/// duplicate operator request selects the force-on-second-signal path.
#[tokio::test]
async fn daemon_shutdown_rpc_requires_control_and_requests_graceful_once() {
    for (capabilities, accepted) in [
        (CapabilitySet::from([Capability::View]), false),
        (
            CapabilitySet::from([Capability::View, Capability::Control]),
            true,
        ),
    ] {
        let (_dir, hub) = liveness_hub().await;
        let (server, client) = UnixStream::pair().expect("socket pair");
        let (shutdown, shutdown_rx, observer) = crate::lifecycle::ShutdownHandle::channel();
        let mut context = liveness_context(hub.clone());
        context.shutdown = shutdown;
        let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
        let serve_task = tokio::spawn(serve(server, context, drain_rx));
        let mut client = PairedClient::new(client);
        client
            .handshake_named_with(haider_client::DAEMON_STOP_CLIENT_NAME, capabilities)
            .await;
        assert!(
            !observer.operator_stop_requested(),
            "a Ready handshake alone must not register completion interest"
        );
        client
            .request("shutdown", RequestBody::DaemonShutdown {})
            .await;

        let response = client.next().await.expect("shutdown response");
        if accepted {
            assert!(matches!(
                response,
                WireFrame::Response {
                    body: ResponseBody::DaemonShutdown {},
                    ..
                }
            ));
            assert!(matches!(
                &*shutdown_rx.borrow(),
                crate::lifecycle::ShutdownRequest::Graceful { .. }
            ));
            assert!(
                observer.operator_stop_requested(),
                "the accepted shutdown RPC registers completion interest"
            );
            client
                .request("shutdown-again", RequestBody::DaemonShutdown {})
                .await;
            assert!(matches!(
                client.next().await,
                Some(WireFrame::Response {
                    body: ResponseBody::DaemonShutdown {},
                    ..
                })
            ));
            assert!(matches!(
                &*shutdown_rx.borrow(),
                crate::lifecycle::ShutdownRequest::Graceful { .. }
            ));
        } else {
            assert!(matches!(
                response,
                WireFrame::Response {
                    body: ResponseBody::Error { ref code, .. },
                    ..
                } if code == haider_rpc::ERROR_CODE_CAPABILITY_DENIED
            ));
            assert!(matches!(
                &*shutdown_rx.borrow(),
                crate::lifecycle::ShutdownRequest::None
            ));
        }

        drop(client);
        let _ = serve_task.await.expect("serve joins");
        hub.shutdown().await.expect("hub shutdown");
    }

    let (_dir, hub) = liveness_hub().await;
    let (server, client) = UnixStream::pair().expect("socket pair");
    let (shutdown, _shutdown_rx, observer) = crate::lifecycle::ShutdownHandle::channel();
    let mut context = liveness_context(hub.clone());
    context.shutdown = shutdown;
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let (_drain_tx, drain_rx) = watch::channel(Some(DrainNotice {
        reason: "preexisting drain".into(),
        instance_id: context.instance_id.clone(),
        daemon_generation: context.daemon_generation,
        deadline_unix_ms: 1,
        deadline,
    }));
    let serve_task = tokio::spawn(serve(server, context, drain_rx));
    let mut client = PairedClient::new(client);
    client
        .handshake_named_with(
            haider_client::DAEMON_STOP_CLIENT_NAME,
            CapabilitySet::from([Capability::View, Capability::Control]),
        )
        .await;
    assert!(
        observer.operator_stop_requested(),
        "a Control-capable stop client joining Draining registers completion interest"
    );
    drop(client);
    let _ = serve_task.await.expect("draining serve joins");
    hub.shutdown().await.expect("hub shutdown");
}

/// MUTATION CHECK: pass `None` instead of `binding_token` from `handle_frame`
/// into `resident_session_binding`. Expected runtime failure: the framed UDS
/// publication reaches the observer but its opaque correlator is missing.
#[tokio::test]
async fn resident_binding_frame_routes_across_the_paired_connection() {
    let (_root, hub) = liveness_hub().await;
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    let observer = hub
        .open_connection(
            CapabilitySet::from([Capability::View, Capability::Control]),
            Arc::new(BindingChannelSink(observed_tx)),
            crate::accounts::ConnectionTransport::LocalSameUid,
        )
        .expect("observer connection");
    let generation = hub.worker_generation();
    assert_eq!(
        observed_rx.recv().await.expect("observer baseline"),
        WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: generation,
            binding_token: None,
        }
    );

    let (server, client) = UnixStream::pair().expect("socketless stream pair");
    let (writers, mut registered_writers) = mpsc::unbounded_channel();
    let context = connection_context(hub.clone(), writers);
    let writer_registry = tokio::spawn(async move {
        while let Some(writer) = registered_writers.recv().await {
            let _ = writer.await;
        }
    });
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve(server, context, drain_rx));
    let mut publisher = PairedClient::new(client);
    publisher.handshake().await;
    publisher
        .send(&WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: generation,
            binding_token: Some("paired-surface".into()),
        })
        .await;

    assert_eq!(
        tokio::time::timeout(haider_platform::TEST_WAIT_TIMEOUT, observed_rx.recv())
            .await
            .expect("binding push is prompt")
            .expect("observer stays open"),
        WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: generation,
            binding_token: Some("paired-surface".into()),
        }
    );

    drop(publisher);
    let _ = serve_task.await.expect("serve task joins");
    writer_registry.await.expect("writer registry joins");
    observer.close().await.expect("observer closes");
    hub.shutdown().await.expect("hub shutdown");
}

#[tokio::test]
async fn msgpack_switches_after_json_welcome_on_paired_uds() {
    let (_dir, hub) = liveness_hub().await;
    let (server, client) = UnixStream::pair().expect("socket pair");
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve(server, liveness_context(hub.clone()), drain_rx));
    let mut client = PairedClient::new(client);

    client.handshake_msgpack().await;
    client
        .request(
            "msgpack-list",
            haider_rpc::RequestBody::SessionList {
                cursor: None,
                limit: 10,
                order: Default::default(),
            },
        )
        .await;
    assert!(matches!(
        client.next().await,
        Some(WireFrame::Response {
            request_id,
            body: haider_rpc::ResponseBody::SessionList { .. },
        }) if request_id.as_str() == "msgpack-list"
    ));

    client
        .stream
        .shutdown()
        .await
        .expect("client write half shuts down");
    assert_eq!(
        serve_task
            .await
            .expect("serve joins")
            .expect("serve result"),
        ConnectionExit::ClosedBeforeDrain {
            reason: ConnectionRetirementReason::Eof,
        }
    );
    hub.shutdown().await.expect("hub shutdown");
}

#[tokio::test]
async fn msgpack_drain_waits_for_json_welcome_on_paired_uds() {
    let (_dir, hub) = liveness_hub().await;
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let (reader, writer) = server.into_split();
    let (write_started, started) = oneshot::channel();
    let release = Arc::new(Notify::new());
    let gated_writer = FirstWriteGate {
        writer,
        gate: Some(Box::pin(Arc::clone(&release).notified_owned())),
        started: Some(write_started),
    };
    let (drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve_io(
        reader,
        gated_writer,
        liveness_context(hub.clone()),
        drain_rx,
    ));

    let hello = WireFrame::Hello(haider_rpc::Hello {
        protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
        protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
        client_name: "paired-drain-order-test".into(),
        client_version: "test".into(),
        client_instance_id: "paired-drain-order-client".into(),
        client_kind: haider_rpc::ClientKind::Headless,
        capabilities_requested: CapabilitySet::from([Capability::View]),
        features_requested: Default::default(),
        max_receive_frame: 1024 * 1024,
        encodings: vec!["msgpack".into()],
    });
    client
        .write_all(&uds_codec::encode(&hello, 1024 * 1024).expect("Hello encodes"))
        .await
        .expect("Hello writes");
    started.await.expect("writer reached the queued Welcome");

    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    drain_tx.send_replace(Some(DrainNotice {
        reason: "test drain".into(),
        instance_id: "instance-liveness".into(),
        daemon_generation: 1,
        deadline_unix_ms: 1,
        deadline,
    }));
    tokio::task::yield_now().await;
    release.notify_one();

    async fn read_one(stream: &mut UnixStream, encoding: haider_rpc::WireEncoding) -> WireFrame {
        let mut prefix = [0_u8; 4];
        stream
            .read_exact(&mut prefix)
            .await
            .expect("frame prefix reads");
        let body_len = usize::try_from(u32::from_be_bytes(prefix)).expect("length fits");
        let mut bytes = prefix.to_vec();
        bytes.resize(4 + body_len, 0);
        stream
            .read_exact(&mut bytes[4..])
            .await
            .expect("frame body reads");
        let mut decoder = uds_codec::Decoder::new(1024 * 1024);
        decoder.set_encoding(encoding);
        let batch = decoder.push(&bytes);
        assert!(batch.error.is_none(), "frame decodes in expected encoding");
        batch.frames.into_iter().next().expect("one frame")
    }

    assert!(matches!(
        read_one(&mut client, haider_rpc::WireEncoding::Json).await,
        WireFrame::Welcome(Welcome {
            encoding: Some(ref encoding),
            ..
        }) if encoding == "msgpack"
    ));
    assert!(matches!(
        read_one(&mut client, haider_rpc::WireEncoding::MessagePack).await,
        WireFrame::ServerDraining { .. }
    ));
    assert_eq!(
        serve_task
            .await
            .expect("serve joins")
            .expect("serve result"),
        ConnectionExit::NoticeDelivered
    );
    hub.shutdown().await.expect("hub shutdown");
}

struct PairedFakeFactory {
    provider: Arc<haider_provider::FakeProvider>,
}

#[async_trait::async_trait]
impl crate::worker::ProviderFactory for PairedFakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &haider_protocol::session::SessionMetadataV1,
    ) -> Result<crate::worker::ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(crate::worker::ResolvedTurnProvider {
            provider: self.provider.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            account_incarnation: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct PairedTurnFixture {
    _root: tempfile::TempDir,
    hub: crate::session_hub::SessionHub,
    manager: crate::worker::WorkerManager,
    fake: Arc<haider_provider::FakeProvider>,
    serve_task: tokio::task::JoinHandle<Result<ConnectionExit, DaemonError>>,
    writer_registry: tokio::task::JoinHandle<()>,
    _drain_tx: watch::Sender<Option<DrainNotice>>,
    client: PairedClient,
    session_id: haider_protocol::ids::SessionId,
    worker_generation: u64,
}

impl PairedTurnFixture {
    async fn start(script: Vec<haider_provider::FakeStep>) -> Self {
        let (root, hub) = liveness_hub().await;
        hub.install_creatable_providers(BTreeSet::from(["fake".to_owned()]))
            .expect("install fake provider name");
        let fake = Arc::new(haider_provider::FakeProvider::new(script));
        let manager = crate::worker::WorkerManager::start(
            hub.clone(),
            crate::worker::WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(PairedFakeFactory {
                    provider: fake.clone(),
                }),
                tool_factory: Arc::new(crate::worker::BrokerToolFactory),
                delegation: None,
                web_search: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("install worker manager");

        let (server, client) = UnixStream::pair().expect("socketless stream pair");
        let (writers, mut registered_writers) = mpsc::unbounded_channel();
        let context = connection_context(hub.clone(), writers);
        let writer_registry = tokio::spawn(async move {
            while let Some(writer) = registered_writers.recv().await {
                let _ = writer.await;
            }
        });
        let (drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
        let serve_task = tokio::spawn(serve(server, context, drain_rx));
        let mut client = PairedClient::new(client);
        client.handshake().await;

        client
            .request(
                "create",
                haider_rpc::RequestBody::SessionCreate {
                    command_id: haider_rpc::CommandId::new("paired-create"),
                    cwd: root.path().to_string_lossy().into_owned(),
                    provider: "fake".into(),
                    model: "fake-v1".into(),
                    max_tokens: 4096,
                },
            )
            .await;
        let (session_id, worker_generation) = loop {
            if let WireFrame::Response {
                body:
                    haider_rpc::ResponseBody::SessionCreate {
                        session_id,
                        worker_generation,
                        ..
                    },
                ..
            } = client.next().await.expect("create response")
            {
                break (session_id, worker_generation);
            }
        };
        client
            .request(
                "attach",
                haider_rpc::RequestBody::SessionAttach {
                    session_id: session_id.clone(),
                    after_seq: 0,
                    mode: haider_rpc::AttachMode::Control,
                    sealed_replay: false,
                },
            )
            .await;
        let mut attached = false;
        let mut caught_up = false;
        while !(attached && caught_up) {
            match client.next().await.expect("attach completes") {
                WireFrame::Response {
                    body: haider_rpc::ResponseBody::SessionAttach { .. },
                    ..
                } => attached = true,
                WireFrame::AttachCaughtUp { .. } => caught_up = true,
                _ => {}
            }
        }

        Self {
            _root: root,
            hub,
            manager,
            fake,
            serve_task,
            writer_registry,
            _drain_tx: drain_tx,
            client,
            session_id,
            worker_generation,
        }
    }

    async fn submit(&mut self, text: &str) {
        self.client
            .request(
                "submit",
                haider_rpc::RequestBody::TurnSubmit {
                    command_id: haider_rpc::CommandId::new("paired-submit"),
                    session_id: self.session_id.clone(),
                    worker_generation: self.worker_generation,
                    text: text.into(),
                    attachments: Vec::new(),
                    mode: haider_protocol::DeliveryMode::Queue,
                },
            )
            .await;
    }

    async fn shutdown(self) {
        let Self {
            _root: root,
            hub,
            manager,
            serve_task,
            writer_registry,
            client,
            ..
        } = self;
        drop(client);
        // Peer-initiated teardown can race writer shutdown into NotConnected
        // on macOS. The task must join; either inner result closes this pair.
        let _ = serve_task.await.expect("serve task joins");
        writer_registry.await.expect("writer registry joins");
        manager.shutdown().await.expect("manager shutdown");
        hub.shutdown().await.expect("hub shutdown");
        drop(root);
    }
}

/// Regression for the misleading symptom seen in the W8a tail: an accepted
/// turn backed by an empty fake script is not a successful empty response.
/// It is a pre-content EOF, so core enters retry backoff; because this raw
/// harness intentionally sends no Ping, the production connection eventually
/// reports `idle_timeout` and closes before retry exhaustion.
#[tokio::test]
async fn empty_fake_turn_retries_until_the_paired_connection_hits_read_idle() {
    let mut fixture = PairedTurnFixture::start(Vec::new()).await;
    tokio::time::pause();
    fixture
        .submit("this fixture has no terminal provider step")
        .await;

    let mut accepted = false;
    let mut retrying = false;
    let mut idle_timeout = false;
    while let Some(frame) = fixture.client.next().await {
        match frame {
            WireFrame::Response {
                body: haider_rpc::ResponseBody::TurnSubmit { .. },
                ..
            } => accepted = true,
            WireFrame::Event { envelope, .. }
                if serde_json::from_value::<haider_protocol::EventPayload>(
                    envelope.payload.clone().into(),
                )
                .is_ok_and(|payload| {
                    matches!(
                        payload,
                        haider_protocol::EventPayload::RunState(
                            haider_protocol::state::RunState::Retrying { .. }
                        )
                    )
                }) =>
            {
                retrying = true;
            }
            WireFrame::ProtocolError(error) if error.code == "idle_timeout" => {
                idle_timeout = true;
            }
            _ => {}
        }
    }

    assert!(accepted, "turn was accepted before the secondary close");
    assert!(retrying, "empty fake stream entered provider retry backoff");
    assert!(idle_timeout, "production liveness caused the observed EOF");
    assert!(
        !fixture.fake.requests().is_empty(),
        "the injected fake was reached; prompt setup did not deadlock"
    );
    fixture.shutdown().await;
}

/// The repaired W8a fixture shape: one explicit provider terminal ends the
/// accepted turn exactly once, and the same connection remains usable.
#[tokio::test]
async fn terminal_fake_turn_finishes_over_the_paired_connection() {
    let mut fixture = PairedTurnFixture::start(vec![haider_provider::FakeStep::Finish {
        reason: haider_protocol::provider::FinishReason::EndTurn,
    }])
    .await;
    fixture.submit("terminal fixture").await;

    let mut accepted = false;
    let mut done = false;
    while !(accepted && done) {
        match fixture.client.next().await.expect("turn reaches terminal") {
            WireFrame::Response {
                body: haider_rpc::ResponseBody::TurnSubmit { .. },
                ..
            } => accepted = true,
            WireFrame::Event { envelope, .. }
                if serde_json::from_value::<haider_protocol::EventPayload>(
                    envelope.payload.clone().into(),
                )
                .is_ok_and(|payload| {
                    payload
                        == haider_protocol::EventPayload::RunState(
                            haider_protocol::state::RunState::Done,
                        )
                }) =>
            {
                done = true;
            }
            _ => {}
        }
    }
    assert_eq!(fixture.fake.requests().len(), 1);

    fixture.client.send(&WireFrame::Ping { nonce: 7 }).await;
    loop {
        if matches!(
            fixture
                .client
                .next()
                .await
                .expect("connection stays open through Pong"),
            WireFrame::Pong { nonce: 7 }
        ) {
            break;
        }
    }
    fixture.shutdown().await;
}

// MUTATION CHECK (R9 server read deadline): remove the liveness tick arm (or
// stop updating `last_read` on reads). Expected failure: this paused-time
// test never observes the close and times out at its outer bound / the
// elapsed window assertion fails.
//
// MUTATION CHECK (usage-ledger timer isolation): revert
// `usage_history_runtime()?.spawn` to `tokio::spawn`. Expected RUNTIME
// failure: the hub's live 15-minute ledger timer captures this connection
// runtime's paused-time auto-advance and the socket close is observed
// hundreds of virtual seconds after the 45-second breach.
#[tokio::test]
async fn silent_negotiated_peer_is_closed_at_the_read_idle_deadline() {
    let (_dir, hub) = liveness_hub().await;
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve(server, liveness_context(hub.clone()), drain_rx));
    handshake_over(&mut client).await;

    // From here the peer is SILENT (never pings, never writes).
    tokio::time::pause();
    let quiet_since = Instant::now();
    let mut decoder = uds_codec::Decoder::new(1024 * 1024);
    let mut closed_with_code = None;
    let mut buffer = [0_u8; 4096];
    loop {
        let read = client.read(&mut buffer).await.expect("read until close");
        if read == 0 {
            break;
        }
        for frame in decoder.push(&buffer[..read]).frames {
            if let haider_rpc::WireFrame::ProtocolError(error) = frame {
                closed_with_code = Some(error.code);
            }
        }
    }
    let elapsed = quiet_since.elapsed();
    // Lower-bound epsilon: the serve task stamps `last_read` at the Hello
    // read a few REAL milliseconds before this test pauses time and
    // captures `quiet_since`, so the virtual close can land that skew
    // early. One second dwarfs any scheduler delay while still pinning the
    // 45 s deadline against the 5 s tick granularity.
    assert!(
        elapsed >= READ_IDLE_DEADLINE - std::time::Duration::from_secs(1)
            && elapsed <= READ_IDLE_DEADLINE + LIVENESS_TICK * 2,
        "close must land at the 45s deadline (tick granularity), was {elapsed:?}"
    );
    assert_eq!(closed_with_code.as_deref(), Some("idle_timeout"));
    let exit = serve_task
        .await
        .expect("serve joins")
        .expect("serve result");
    assert_eq!(
        exit,
        ConnectionExit::ClosedBeforeDrain {
            reason: ConnectionRetirementReason::Error,
        }
    );
    let _ = hub.shutdown().await;
}

// MUTATION CHECK (R9 liveness reset): make `last_read` never reset (treat
// pings as non-activity). Expected failure: the pinging peer below is closed
// around 45s instead of staying attached for five virtual minutes.
#[tokio::test]
async fn pinging_peer_stays_attached_across_quiescence() {
    let (_dir, hub) = liveness_hub().await;
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
    let serve_task = tokio::spawn(serve(server, liveness_context(hub.clone()), drain_rx));
    handshake_over(&mut client).await;

    tokio::time::pause();
    use tokio::io::AsyncWriteExt;
    let mut decoder = uds_codec::Decoder::new(1024 * 1024);
    let mut pongs = 0_u64;
    // Five virtual minutes of nothing but the R9 heartbeat cadence.
    for nonce in 0..20_u64 {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let ping = uds_codec::encode(&haider_rpc::WireFrame::Ping { nonce }, 1024 * 1024)
            .expect("ping encodes");
        client.write_all(&ping).await.expect("ping writes");
        let mut buffer = [0_u8; 1024];
        loop {
            let read = client.read(&mut buffer).await.expect("pong reads");
            assert_ne!(read, 0, "server must not close a pinging peer");
            let frames = decoder.push(&buffer[..read]).frames;
            if frames
                .iter()
                .any(|frame| matches!(frame, haider_rpc::WireFrame::Pong { .. }))
            {
                pongs += 1;
                break;
            }
        }
    }
    assert_eq!(pongs, 20);
    assert!(
        !serve_task.is_finished(),
        "connection must still be serving"
    );
    drop(client);
    // Peer-initiated teardown: macOS reports the racing writer shutdown as
    // NotConnected; either shape ends the task, which is the law under test.
    let _ = serve_task.await.expect("serve joins");
    let _ = hub.shutdown().await;
}

/// The TOTAL Welcome barrier (v0.0.934 wire fix): while the JSON Welcome
/// is queued, no OTHER lane pops — a frame admitted after negotiation may
/// already be MessagePack encoded, and a client decodes JSON until it has
/// seen the Welcome. Only the Welcome's own FIFO lane advances (its
/// earlier frames are pre-switch JSON by construction).
///
/// MUTATION CHECK: restore the unguarded round-robin pop while
/// `welcome_queued`. Expected runtime failure: `event-before` — pushed
/// FIRST overall, on another lane — is returned before the Welcome below.
#[tokio::test]
async fn welcome_barrier_holds_every_other_lane_until_the_welcome_writes() {
    let lane = OutboundLane::new(8, 4_096, 512);
    let other = LaneKey::Attachment(AttachmentId::new("early-events"));
    lane.try_push(other.clone(), ordinary(b"event-before"))
        .expect("event queues");
    lane.try_push(LaneKey::System, ordinary(b"pre-welcome-json"))
        .expect("pre-welcome queues");
    lane.try_push(
        LaneKey::System,
        QueuedFrame::welcome(b"the-welcome".to_vec()),
    )
    .expect("welcome queues");

    assert_eq!(
        lane.recv().await.expect("first").bytes,
        b"pre-welcome-json",
        "the welcome's own lane advances in FIFO order"
    );
    let second = lane.recv().await.expect("second");
    assert_eq!(second.bytes, b"the-welcome");
    assert!(second.welcome);
    assert_eq!(
        lane.recv().await.expect("third").bytes,
        b"event-before",
        "the held lane flows only after the welcome popped"
    );
}

#[test]
fn gate_f2_legacy_event_frame_strips_metrics_activity() {
    use haider_protocol::activity::{ActivityKind, ActivityStateV1, ActivityTouchV1};
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{AgentId, DeviceId, EventId, RunId, SessionId};
    let snapshot = haider_protocol::agent::AgentMetricsSnapshot {
        agent: Some(AgentId::new("repro-agent")),
        session_id: SessionId::new("repro-child"),
        head_seq: 9,
        started_at_ms: 1,
        terminal_at_ms: None,
        live: true,
        tool_attempts: 2,
        usage: None,
        activity: Some(ActivityStateV1 {
            open: vec![ActivityKind::Edit],
            touched: vec![ActivityTouchV1 {
                kind: ActivityKind::Edit,
                at_ms: 1,
            }],
            latest: Some(ActivityTouchV1 {
                kind: ActivityKind::Edit,
                at_ms: 1,
            }),
            terminal: false,
        }),
    };
    let envelope: haider_protocol::envelope::RawEnvelope = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("delegation-metrics-repro-agent-9"),
        seq: 12,
        session_id: SessionId::new("repro-parent"),
        branch_id: None,
        run_id: Some(RunId::new("repro-parent-run")),
        agent_id: None,
        device_id: DeviceId::new("repro-device"),
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
        payload: snapshot.to_payload_value().expect("payload").into(),
    };
    let frame = WireFrame::Event {
        attachment_id: AttachmentId::new("repro-attachment"),
        session_id: SessionId::new("repro-parent"),
        envelope: envelope.clone(),
    };
    let descendant = WireFrame::SessionDescendantStream {
        attachment_id: AttachmentId::new("repro-attachment"),
        event: haider_rpc::SessionDescendantStreamEventWire::Envelope {
            session_id: SessionId::new("repro-child"),
            agent_id: AgentId::new("repro-agent"),
            envelope: envelope.clone(),
        },
    };
    let read = WireFrame::Response {
        request_id: RequestId::new("read"),
        body: ResponseBody::SessionRead {
            result: haider_rpc::SessionReadResult {
                session_id: SessionId::new("repro-child"),
                range: haider_rpc::SeqRange {
                    start_seq: 12,
                    end_seq: 12,
                },
                head_seq: 12,
                metadata: None,
                latest_context_footprint: None,
                envelopes: vec![envelope],
            },
        },
    };
    for frame in [frame, descendant, read] {
        for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
            let mut legacy = frame.clone();
            omit_activity(&mut legacy);
            omit_fleet_identity(&mut legacy);
            let json = serde_json::to_string(&legacy).expect("serializes");
            assert!(
                !json.contains("\"activit"),
                "legacy event frame leaked metrics activity ({encoding:?}): {json}"
            );
            assert!(
                serde_json::to_string(&frame)
                    .expect("activity frame fixture")
                    .contains("\"activity\"")
            );
            let bytes = uds_codec::encode_with(&legacy, 1_048_576, encoding).expect("encodes");
            assert!(!bytes.windows(7).any(|part| part == b"activit"));
            let sink = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: false,
                activity: false,
            };
            let sent = sink.encode(&frame).expect("legacy sink encodes");
            let actual = match sent {
                OutboundBytes::Contiguous(bytes) => bytes.as_slice().to_vec(),
                OutboundBytes::Framed(frame) => {
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(frame.prefix());
                    bytes.extend_from_slice(frame.body());
                    bytes
                }
                OutboundBytes::Event(frame) => {
                    let mut bytes = Vec::new();
                    frame.write_to(&mut bytes).expect("writes event");
                    bytes
                }
            };
            assert_eq!(actual, bytes, "sink preserves pre-lane bytes");
            let mut identity_only = frame.clone();
            omit_activity(&mut identity_only);
            let class2 = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: true,
                activity: false,
            };
            assert_eq!(
                outbound_bytes(class2.encode(&frame).expect("class 2 event encodes")),
                uds_codec::encode_with(&identity_only, 1_048_576, encoding)
                    .expect("old event encodes")
            );
            let class3 = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity: true,
                activity: true,
            };
            assert_eq!(
                outbound_bytes(class3.encode(&frame).expect("class 3 event encodes")),
                uds_codec::encode_with(&frame, 1_048_576, encoding).expect("full event encodes")
            );
            if let WireFrame::Event {
                attachment_id,
                session_id,
                envelope,
            } = &frame
            {
                let prepared = sink
                    .prepare_event(attachment_id, session_id, envelope)
                    .expect("attachment Event encodes");
                let encoded = prepared.encoded_bytes().expect("prepared bytes");
                let actual = match encoded {
                    OutboundBytes::Contiguous(bytes) => bytes.as_slice().to_vec(),
                    OutboundBytes::Framed(frame) => {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(frame.prefix());
                        bytes.extend_from_slice(frame.body());
                        bytes
                    }
                    OutboundBytes::Event(frame) => {
                        let mut bytes = Vec::new();
                        frame.write_to(&mut bytes).expect("writes event");
                        bytes
                    }
                };
                assert_eq!(
                    actual, bytes,
                    "prepared attach/replay Event is legacy identical"
                );
                let prepared_class2 = class2
                    .prepare_event(attachment_id, session_id, envelope)
                    .expect("class 2 prepared event");
                assert_eq!(
                    outbound_bytes(
                        prepared_class2
                            .encoded_bytes()
                            .expect("prepared bytes")
                            .clone()
                    ),
                    uds_codec::encode_with(&identity_only, 1_048_576, encoding)
                        .expect("old event encodes")
                );
                let prepared_class3 = class3
                    .prepare_event(attachment_id, session_id, envelope)
                    .expect("class 3 prepared event");
                assert_eq!(
                    outbound_bytes(
                        prepared_class3
                            .encoded_bytes()
                            .expect("prepared bytes")
                            .clone()
                    ),
                    uds_codec::encode_with(&frame, 1_048_576, encoding)
                        .expect("full event encodes")
                );
            }
        }
    }
}

#[test]
fn legacy_event_strips_typed_tool_activity_but_modern_keeps_it() {
    use haider_protocol::activity::ActivityKind;
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EventId, ItemId, SessionId};
    use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
    let payload = haider_protocol::EventPayload::Item(ItemEvent::Completed {
        item_id: ItemId::new("tool"),
        item: TurnItem::ToolCall {
            call_id: "tool".into(),
            name: "computer".into(),
            args: serde_json::Value::Null,
            status: ToolStatus::Completed,
            activity: Some(ActivityKind::Read),
        },
    });
    let frame = WireFrame::Event {
        attachment_id: AttachmentId::new("a"),
        session_id: SessionId::new("s"),
        envelope: EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("e"),
            seq: 1,
            session_id: SessionId::new("s"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("d"),
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
            payload: serde_json::to_value(payload)
                .expect("activity frame fixture")
                .into(),
        },
    };
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        let mut legacy = frame.clone();
        omit_activity(&mut legacy);
        omit_fleet_identity(&mut legacy);
        let modern = serde_json::to_string(&frame).expect("activity frame fixture");
        let old = serde_json::to_string(&legacy).expect("activity frame fixture");
        assert!(modern.contains("\"activity\":\"read\""));
        assert!(!old.contains("\"activit"));
        let bytes =
            uds_codec::encode_with(&legacy, 1_048_576, encoding).expect("activity frame fixture");
        assert!(!bytes.windows(7).any(|part| part == b"activit"));
        let class2 = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: false,
        };
        assert_eq!(
            outbound_bytes(class2.encode(&frame).expect("old tool shape")),
            bytes
        );
        let class3 = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: true,
        };
        assert_eq!(
            outbound_bytes(class3.encode(&frame).expect("full tool shape")),
            uds_codec::encode_with(&frame, 1_048_576, encoding).expect("full tool bytes")
        );
    }
}

#[test]
fn legacy_recovery_event_keeps_the_previous_network_shape() {
    use haider_protocol::effect::{EffectClass, EffectIntent, EffectPhase};
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EffectId, EventId, SessionId};
    let intent = EffectIntent {
        effect: EffectId::new("provider-admission-r-1"),
        class: EffectClass::ProviderTransport,
        summary: "provider request attempt 1 admitted before daemon restart".into(),
        args_digest: "sha256:0".into(),
        workspace_revision: None,
    };
    let event = |class| WireFrame::Event {
        attachment_id: AttachmentId::new("a"),
        session_id: SessionId::new("s"),
        envelope: EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("e"),
            seq: 1,
            session_id: SessionId::new("s"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("d"),
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
            payload: serde_json::to_value(haider_protocol::EventPayload::Effect(
                EffectPhase::Intent(EffectIntent {
                    class,
                    ..intent.clone()
                }),
            ))
            .expect("activity frame fixture")
            .into(),
        },
    };
    let modern = event(EffectClass::ProviderTransport);
    let historical = event(EffectClass::Network {
        host: "provider-request".into(),
    });
    let mut legacy = modern.clone();
    omit_activity(&mut legacy);
    omit_fleet_identity(&mut legacy);
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        assert_eq!(
            uds_codec::encode_with(&legacy, 1_048_576, encoding).expect("activity frame fixture"),
            uds_codec::encode_with(&historical, 1_048_576, encoding)
                .expect("activity frame fixture"),
        );
        let identity_only = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: false,
        };
        assert_eq!(
            outbound_bytes(identity_only.encode(&modern).expect("class 2 transport")),
            uds_codec::encode_with(&historical, 1_048_576, encoding).expect("historical transport")
        );
        let no_identity = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: false,
            activity: false,
        };
        assert_eq!(
            outbound_bytes(no_identity.encode(&modern).expect("class 1 transport")),
            uds_codec::encode_with(&historical, 1_048_576, encoding).expect("historical transport")
        );
        let full = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: true,
        };
        assert_eq!(
            outbound_bytes(full.encode(&modern).expect("class 3 transport")),
            uds_codec::encode_with(&modern, 1_048_576, encoding).expect("full transport")
        );
    }
    assert!(
        serde_json::to_string(&modern)
            .expect("activity frame fixture")
            .contains("provider_transport")
    );
}

#[test]
fn operator_checkpoint_effect_keeps_the_previous_fs_write_shape_for_older_peers() {
    use haider_protocol::effect::{EffectClass, EffectIntent, EffectPhase};
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EffectId, EventId, SessionId};
    let event = |class| WireFrame::Event {
        attachment_id: AttachmentId::new("a"),
        session_id: SessionId::new("s"),
        envelope: EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("e"),
            seq: 1,
            session_id: SessionId::new("s"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("d"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 1,
            render: RenderTargets {
                ui: false,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(haider_protocol::EventPayload::Effect(
                EffectPhase::Intent(EffectIntent {
                    effect: EffectId::new("operator"),
                    class,
                    summary: "undo workspace checkpoint".into(),
                    args_digest: "digest".into(),
                    workspace_revision: None,
                }),
            ))
            .expect("effect")
            .into(),
        },
    };
    let modern = event(EffectClass::CheckpointRestore);
    let old = event(EffectClass::FsWrite);
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        for fleet_identity in [false, true] {
            let sink = ConnectionFrameSink {
                lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                outbound_limit: 1_048_576,
                encoding,
                fleet_identity,
                activity: false,
            };
            assert_eq!(
                outbound_bytes(sink.encode(&modern).expect("old checkpoint frame")),
                uds_codec::encode_with(&old, 1_048_576, encoding).expect("old FsWrite frame")
            );
        }
        let modern_sink = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: true,
        };
        assert_eq!(
            outbound_bytes(modern_sink.encode(&modern).expect("new checkpoint frame")),
            uds_codec::encode_with(&modern, 1_048_576, encoding).expect("new checkpoint bytes")
        );
    }
}

#[test]
fn legacy_running_tool_event_omits_live_activity_metadata() {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    let base = serde_json::json!({"type":"run_state","state":"running_tool"});
    let mut rich = base.clone();
    rich["activity_kind"] = serde_json::json!("read");
    rich["activity_call_id"] = serde_json::json!("registered");
    let frame = |payload: serde_json::Value| WireFrame::Event {
        attachment_id: AttachmentId::new("a"),
        session_id: SessionId::new("s"),
        envelope: EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("e"),
            seq: 1,
            session_id: SessionId::new("s"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("d"),
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
            payload: payload.into(),
        },
    };
    let modern = frame(rich);
    let historical = frame(base);
    let mut legacy = modern.clone();
    omit_activity(&mut legacy);
    omit_fleet_identity(&mut legacy);
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        assert_eq!(
            uds_codec::encode_with(&legacy, 1_048_576, encoding).expect("activity frame fixture"),
            uds_codec::encode_with(&historical, 1_048_576, encoding)
                .expect("activity frame fixture"),
        );
        let class2 = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: false,
        };
        assert_eq!(
            outbound_bytes(class2.encode(&modern).expect("old run-state shape")),
            uds_codec::encode_with(&historical, 1_048_576, encoding).expect("old run-state bytes")
        );
        let class3 = ConnectionFrameSink {
            lane: OutboundLane::new(1, 1_048_576, 1_048_576),
            outbound_limit: 1_048_576,
            encoding,
            fleet_identity: true,
            activity: true,
        };
        assert_eq!(
            outbound_bytes(class3.encode(&modern).expect("full run-state shape")),
            uds_codec::encode_with(&modern, 1_048_576, encoding).expect("full run-state bytes")
        );
    }
    assert!(
        serde_json::to_string(&modern)
            .expect("activity frame fixture")
            .contains("activity_kind")
    );
}

#[test]
fn activity_peer_classes_preserve_historical_bytes_on_every_envelope_path() {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{AgentId, DeviceId, EventId, SessionId};
    let metrics = identity_gate_metrics();
    let mut old_metrics = metrics.clone();
    old_metrics.activity = None;
    let payloads = [
        (
            metrics.to_payload_value().expect("rich metrics"),
            old_metrics.to_payload_value().expect("old metrics"),
        ),
        (
            serde_json::json!({"type":"item", "event":"completed", "item_id":"i", "item":{"item":"tool_call", "call_id":"c", "name":"computer", "args":{}, "status":"completed", "activity":"read"}}),
            serde_json::json!({"type":"item", "event":"completed", "item_id":"i", "item":{"item":"tool_call", "call_id":"c", "name":"computer", "args":{}, "status":"completed"}}),
        ),
        (
            serde_json::json!({"type":"run_state", "state":"running_tool", "activity_kind":"read", "activity_call_id":"c"}),
            serde_json::json!({"type":"run_state", "state":"running_tool"}),
        ),
        (
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"provider_transport"}, "summary":"recovery", "args_digest":"digest"}),
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"network", "host":"provider-request"}, "summary":"recovery", "args_digest":"digest"}),
        ),
        (
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"checkpoint_restore"}, "summary":"undo", "args_digest":"digest"}),
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"fs_write"}, "summary":"undo", "args_digest":"digest"}),
        ),
        (
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"network", "host":"provider-request"}, "effect_origin":"provider_transport", "summary":"recovery", "args_digest":"digest"}),
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"network", "host":"provider-request"}, "summary":"recovery", "args_digest":"digest"}),
        ),
        (
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"fs_write"}, "effect_origin":"checkpoint_restore", "summary":"undo", "args_digest":"digest"}),
            serde_json::json!({"type":"effect", "phase":"intent", "effect":"e", "class":{"class":"fs_write"}, "summary":"undo", "args_digest":"digest"}),
        ),
        // Negative control: activity-like keys inside model arguments are data.
        (
            serde_json::json!({"type":"item", "event":"completed", "item_id":"i", "item":{"item":"tool_call", "call_id":"c", "name":"custom", "args":{"activity":"keep", "activity_kind":"keep"}, "status":"completed"}}),
            serde_json::json!({"type":"item", "event":"completed", "item_id":"i", "item":{"item":"tool_call", "call_id":"c", "name":"custom", "args":{"activity":"keep", "activity_kind":"keep"}, "status":"completed"}}),
        ),
    ];
    let envelope = |payload: serde_json::Value| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("matrix-event"),
        seq: 1,
        session_id: SessionId::new("child"),
        branch_id: None,
        run_id: None,
        agent_id: Some(AgentId::new("child-agent")),
        device_id: DeviceId::new("device"),
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
        payload: payload.into(),
    };
    let frames = |envelope: haider_protocol::envelope::RawEnvelope| {
        [
            WireFrame::Event {
                attachment_id: AttachmentId::new("a"),
                session_id: envelope.session_id.clone(),
                envelope: envelope.clone(),
            },
            WireFrame::SessionDescendantStream {
                attachment_id: AttachmentId::new("a"),
                event: haider_rpc::SessionDescendantStreamEventWire::Envelope {
                    session_id: envelope.session_id.clone(),
                    agent_id: AgentId::new("child-agent"),
                    envelope: envelope.clone(),
                },
            },
            WireFrame::Response {
                request_id: RequestId::new("read"),
                body: ResponseBody::SessionRead {
                    result: haider_rpc::SessionReadResult {
                        session_id: envelope.session_id.clone(),
                        range: haider_rpc::SeqRange {
                            start_seq: 1,
                            end_seq: 1,
                        },
                        head_seq: 1,
                        metadata: None,
                        latest_context_footprint: None,
                        envelopes: vec![envelope],
                    },
                },
            },
        ]
    };
    for (rich, old) in payloads {
        let rich_frames = frames(envelope(rich));
        let old_frames = frames(envelope(old));
        for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
            for (fleet_identity, activity) in [(false, false), (true, false), (true, true)] {
                let sink = ConnectionFrameSink {
                    lane: OutboundLane::new(1, 1_048_576, 1_048_576),
                    outbound_limit: 1_048_576,
                    encoding,
                    fleet_identity,
                    activity,
                };
                for (rich_frame, old_frame) in rich_frames.iter().zip(&old_frames) {
                    let expected = if activity { rich_frame } else { old_frame };
                    let bytes = uds_codec::encode_with(expected, 1_048_576, encoding)
                        .expect("independent expected bytes");
                    assert_eq!(
                        outbound_bytes(sink.encode(rich_frame).expect("outbound frame")),
                        bytes,
                        "{encoding:?}, identity={fleet_identity}, activity={activity}"
                    );
                    if let WireFrame::Event {
                        attachment_id,
                        session_id,
                        envelope,
                    } = rich_frame
                    {
                        let prepared = sink
                            .prepare_event(attachment_id, session_id, envelope)
                            .expect("prepared replay");
                        assert_eq!(
                            outbound_bytes(
                                prepared.encoded_bytes().expect("prepared bytes").clone()
                            ),
                            bytes
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn activity_handshake_opt_in_never_leaks_through_the_standard_welcome_cache() {
    let (_root, hub) = liveness_hub().await;
    let context = liveness_context(hub);
    let (_sender, drain) = watch::channel(None);
    for requested in [false, true, false, true] {
        for encodings in [Vec::new(), vec!["msgpack".to_owned()]] {
            let lane = OutboundLane::new(1, 1_048_576, 1_048_576);
            let mut grant = None;
            let hello = Hello {
                protocol_min: 1,
                protocol_max: 1,
                client_name: "activity-reader".into(),
                client_version: "0.0.973".into(),
                client_instance_id: "client".into(),
                client_kind: haider_rpc::ClientKind::Tui,
                capabilities_requested: [Capability::View, Capability::Control]
                    .into_iter()
                    .collect(),
                features_requested: if requested {
                    [FEATURE_SESSION_ACTIVITY_V1.to_owned()]
                        .into_iter()
                        .collect()
                } else {
                    Default::default()
                },
                max_receive_frame: 1_048_576,
                encodings,
            };
            assert!(
                negotiate_hello(hello, &context, &drain, &lane, &mut grant, 1_048_576)
                    .expect("handshake")
                    .is_some()
            );
            let granted = grant.expect("connection grant");
            assert_eq!(
                granted.features.contains(FEATURE_SESSION_ACTIVITY_V1),
                requested
            );
            let welcome = lane.recv().await.expect("welcome");
            let bytes = outbound_bytes(welcome.bytes);
            let frames = uds_codec::Decoder::new(1_048_576).push(&bytes).frames;
            assert!(
                matches!(frames.as_slice(), [WireFrame::Welcome(w)] if w.features.contains(FEATURE_SESSION_ACTIVITY_V1) == requested)
            );
        }
    }
}
