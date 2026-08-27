//! Private fair-outbox, reply-floor, and drain-order tests.
//!
//! This module is crate-internal only because `OutboundLane`, `LaneKey`, and
//! `run_writer` intentionally are not production API.

// Keep this transport-specific suite unavailable off Unix even if its parent
// module declaration is accidentally broadened in the future.
#![cfg(unix)]
#![allow(clippy::expect_used)]

use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixStream, unix::OwnedWriteHalf};

struct FirstWriteGate {
    writer: OwnedWriteHalf,
    gate: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    started: Option<oneshot::Sender<()>>,
}

impl AsyncWrite for FirstWriteGate {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
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
/// MUTATION CHECK: remove `FEATURE_MONITOR_CONTROL_V1` or
/// `FEATURE_MONITOR_DELIVERY_V1`. Expected runtime failure: clients cannot
/// distinguish the served typed control/replay surfaces from the private APK
/// transport or from total client-surface absence.
///
/// MUTATION CHECK: remove `FEATURE_WORKFLOW_CATALOG_V1` or
/// `FEATURE_LOOM_PIPE_DAG_V1`. Expected runtime failure: clients cannot
/// discover the catalog snapshot or distinguish v0.0.961 fork/join/back-edge
/// grammar from legacy `loom_v1`.
#[test]
fn welcome_features_pin_served_management_families() {
    assert_eq!(
        welcome_features().len(),
        87,
        "the ordinary Welcome advertises all 85 prior and two v0.0.963 headless feature tokens"
    );
    assert_eq!(
        welcome_features(),
        BTreeSet::from([
            haider_rpc::FEATURE_AGENT_MESSAGE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned(),
            FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_SOURCES_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
            FEATURE_ACCOUNT_ROTATION_V1.to_owned(),
            haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned(),
            haider_rpc::FEATURE_EXPORT_SEQ_V1.to_owned(),
            haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FORK_V1.to_owned(),
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
            haider_rpc::FEATURE_HAIDER_CODE_PLAN_STATUS_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_CLI_PRESENCE_V1.to_owned(),
            haider_rpc::FEATURE_TYPED_AGENT_INSTALL_V1.to_owned(),
            haider_rpc::FEATURE_TYPED_AGENT_INSTALL_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_RUN_ID_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_V1.to_owned(),
            haider_rpc::FEATURE_LOOM_PIPE_DAG_V1.to_owned(),
            haider_rpc::FEATURE_WORKFLOW_CATALOG_V1.to_owned(),
            haider_rpc::FEATURE_WORKFLOW_INSTANCE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_WORKFLOW_STATE_V1.to_owned(),
            haider_rpc::FEATURE_MODELS_LIST_V1.to_owned(),
            haider_rpc::FEATURE_PIPE_NATIVE_V2.to_owned(),
            haider_rpc::FEATURE_PIPE_TOOL_STATUS_V1.to_owned(),
            haider_rpc::FEATURE_INPUT_MIRROR_ATTACHMENTS_V1.to_owned(),
            haider_rpc::FEATURE_INPUT_MIRROR_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_ATTACH_SEALED_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_CONFIG_V1.to_owned(),
            haider_rpc::FEATURE_WIRE_MSGPACK_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LIST_WATCH_V1.to_owned(),
            FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
            FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
            FEATURE_PROVIDER_MODELS_V1.to_owned(),
            FEATURE_PROVIDER_REMOVE_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_SESSION_BINDING_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1.to_owned(),
            haider_rpc::FEATURE_RUN_RETRY_V1.to_owned(),
            haider_rpc::FEATURE_RUN_BUDGET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned(),
            FEATURE_SESSION_MUTATION_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_RENAME_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_SEEN_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_NEEDS_INPUT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_LIST_WATCH_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_LABEL_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FLEET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_DESCENDANT_STREAM_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_V1.to_owned(),
            FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
            haider_rpc::FEATURE_AUTONOMOUS_INTERACTION_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_SEGMENT_STRUCTURED_V1.to_owned(),
            haider_rpc::FEATURE_STATUS_SEGMENT_V1.to_owned(),
            haider_rpc::FEATURE_STORE_HEALTH_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_BATCH_V1.to_owned(),
            haider_rpc::FEATURE_RESIDENT_TURN_SUBMIT_V1.to_owned(),
            haider_rpc::FEATURE_TUI_ATTACH_ANNOUNCE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_LINEAGE_V1.to_owned(),
            FEATURE_SHELL_EXEC_V1.to_owned(),
            haider_rpc::FEATURE_USER_COMMAND_V1.to_owned(),
            FEATURE_TOOL_INVENTORY_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_MONITOR_DELIVERY_V1.to_owned(),
            haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned(),
            FEATURE_TURN_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned(),
            haider_rpc::FEATURE_USAGE_HISTORY_V1.to_owned(),
            haider_rpc::FEATURE_QUEUE_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_COMPUTER_PERMISSION_ACTIONS_V1.to_owned(),
            FEATURE_VAULT_STAGE_V1.to_owned(),
        ])
    );
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

/// MUTATION CHECK: replace `welcome.user_command_withheld = true` in the
/// fallback with `false`. Expected runtime failure: the tight Welcome still
/// omits `user_command_v1` but loses the causal marker pinned below.
#[test]
fn tight_welcome_omits_only_the_additive_user_command_feature() {
    let welcome = Welcome {
        protocol: haider_rpc::WIRE_PROTOCOL_VERSION,
        instance_id: "instance".into(),
        daemon_generation: 7,
        frame_limit: 1_024,
        profile_id: "profile".into(),
        daemon_version: "test".into(),
        lifecycle_phase: LifecyclePhase::Ready,
        capabilities_granted: CapabilitySet::from([Capability::View, Capability::Control]),
        features: welcome_features(),
        user_command_withheld: false,
        encoding: None,
    };
    let full = uds_codec::encode(&WireFrame::Welcome(welcome.clone()), usize::MAX)
        .expect("full Welcome encodes");
    let full_body_len = full.len() - 4;

    let ample = encode_welcome_for_peer(welcome.clone(), full_body_len)
        .expect("exact full limit carries every feature");
    assert_eq!(ample.as_slice(), full.as_slice());
    assert!(
        !std::str::from_utf8(&ample[4..])
            .expect("Welcome is JSON")
            .contains("\"uw\""),
        "nothing withheld serializes no marker, never false"
    );
    let mut ample_decoder = uds_codec::Decoder::new(full_body_len);
    let ample_frames = ample_decoder.push(&ample).frames;
    assert!(matches!(
        ample_frames.as_slice(),
        [WireFrame::Welcome(Welcome { features, user_command_withheld, .. })]
            if features == &welcome.features && !user_command_withheld
    ));

    let tight = encode_welcome_for_peer(welcome.clone(), full_body_len - 1)
        .expect("tight peer retains the pre-T4 handshake");
    let mut without_feature = welcome.clone();
    assert!(without_feature.features.remove(FEATURE_USER_COMMAND_V1));
    let without_feature = uds_codec::encode(&WireFrame::Welcome(without_feature), usize::MAX)
        .expect("unmarked Welcome without user command encodes");
    let without_feature_body_len = without_feature.len() - 4;
    let tight_body_len = tight.len() - 4;
    assert_eq!(
        full_body_len - without_feature_body_len,
        18,
        "the real encoder's feature-array element costs 18 bytes"
    );
    assert_eq!(
        tight_body_len - without_feature_body_len,
        10,
        "the short true marker costs 10 bytes"
    );
    assert_eq!(
        full_body_len - tight_body_len,
        8,
        "replacement must be strictly smaller than the withheld token"
    );
    assert!(
        std::str::from_utf8(&tight[4..])
            .expect("Welcome is JSON")
            .contains("\"uw\":true"),
        "a withheld token carries the causal marker"
    );
    let mut tight_decoder = uds_codec::Decoder::new(full_body_len - 1);
    let tight_frames = tight_decoder.push(&tight).frames;
    let mut expected = welcome.features.clone();
    assert!(expected.remove(FEATURE_USER_COMMAND_V1));
    assert!(matches!(
        tight_frames.as_slice(),
        [WireFrame::Welcome(Welcome { features, user_command_withheld, .. })]
            if features == &expected && *user_command_withheld
    ));

    let mut selected_encoding = welcome;
    selected_encoding.encoding = Some("msgpack".into());
    let selected_full =
        uds_codec::encode(&WireFrame::Welcome(selected_encoding.clone()), usize::MAX)
            .expect("selected-encoding Welcome encodes");
    let selected_full_body_len = selected_full.len() - 4;
    let mut selected_unmarked = selected_encoding.clone();
    assert!(selected_unmarked.features.remove(FEATURE_USER_COMMAND_V1));
    let selected_unmarked = uds_codec::encode(&WireFrame::Welcome(selected_unmarked), usize::MAX)
        .expect("selected-encoding unmarked Welcome encodes");
    let selected_tight = encode_welcome_for_peer(selected_encoding, selected_full_body_len - 1)
        .expect("selected-encoding fallback fits");
    assert_eq!(selected_full_body_len - (selected_unmarked.len() - 4), 18);
    assert_eq!(
        (selected_tight.len() - 4) - (selected_unmarked.len() - 4),
        10
    );
    assert_eq!(selected_full_body_len - (selected_tight.len() - 4), 8);
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
            payload: serde_json::json!({"type": "budget_boundary"}),
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
        self.send(&WireFrame::Hello(haider_rpc::Hello {
            protocol_min: haider_rpc::WIRE_PROTOCOL_VERSION,
            protocol_max: haider_rpc::WIRE_PROTOCOL_VERSION,
            client_name: "paired-connection-test".into(),
            client_version: "test".into(),
            client_instance_id: "paired-client".into(),
            client_kind: haider_rpc::ClientKind::Headless,
            capabilities_requested: capabilities,
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

/// MUTATION CHECK: remove the Control gate or the single ShutdownHandle
/// request in `handle_frame`. Expected runtime failure: the view-only peer
/// starts draining, or the control peer never advances the lifecycle watch.
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
        let (shutdown, shutdown_rx, _observer) = crate::lifecycle::ShutdownHandle::channel();
        let mut context = liveness_context(hub.clone());
        context.shutdown = shutdown;
        let (_drain_tx, drain_rx) = watch::channel(Option::<DrainNotice>::None);
        let serve_task = tokio::spawn(serve(server, context, drain_rx));
        let mut client = PairedClient::new(client);
        client.handshake_with(capabilities).await;
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
        tokio::time::timeout(std::time::Duration::from_secs(1), observed_rx.recv())
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
        ConnectionExit::ClosedBeforeDrain
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
                    envelope.payload.clone(),
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
                    envelope.payload.clone(),
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
    assert_eq!(exit, ConnectionExit::ClosedBeforeDrain);
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
