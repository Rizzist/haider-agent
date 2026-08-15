//! Private fair-outbox, reply-floor, and drain-order tests.
//!
//! This module is crate-internal only because `OutboundLane`, `LaneKey`, and
//! `run_writer` intentionally are not production API.

#![allow(clippy::expect_used)]

use super::*;
use tokio::io::AsyncReadExt;

fn ordinary(bytes: &[u8]) -> QueuedFrame {
    QueuedFrame::ordinary(bytes.to_vec())
}

fn staged_response(attachment: &AttachmentId, request: &str, bytes: &[u8]) -> QueuedFrame {
    QueuedFrame {
        bytes: bytes.to_vec().into(),
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
/// MUTATION CHECK: remove `FEATURE_CONTEXT_COMPACTION_V1`. Expected runtime
/// failure: clients cannot discover the served `session.compact` method.
/// Verified by revert on 2026-07-30.
///
/// MUTATION CHECK: remove `FEATURE_SHELL_EXEC_V1` or
/// `FEATURE_TOOL_INVENTORY_V1`. Expected runtime failure: the exact feature
/// set no longer advertises one of the W8a RPC methods the daemon serves.
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
/// MUTATION CHECK: remove `FEATURE_SESSION_OBSERVE_V1`. Expected RUNTIME
/// failure: scriptable clients cannot discover the served state digest.
///
/// MUTATION CHECK: remove `FEATURE_SESSION_FLEET_V1`. Expected RUNTIME
/// failure: fleet clients cannot discover the served descendant snapshot.
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
/// MUTATION CHECK: remove `FEATURE_SESSION_RENAME_V1`. Expected RUNTIME
/// failure: clients cannot discover the served receipted `session.rename`
/// surface (G2) and the TUI keeps its stale-daemon notice forever.
#[test]
fn welcome_features_pin_served_management_families() {
    assert_eq!(
        welcome_features(),
        BTreeSet::from([
            haider_rpc::FEATURE_AGENT_MESSAGE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1.to_owned(),
            FEATURE_ACCOUNT_LOGIN_API_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_IMPORT_V1.to_owned(),
            haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1.to_owned(),
            FEATURE_ACCOUNT_ROTATION_V1.to_owned(),
            haider_rpc::FEATURE_ARTIFACT_PUT_V1.to_owned(),
            haider_rpc::FEATURE_BRANCH_CREATE_V1.to_owned(),
            FEATURE_CONTEXT_COMPACTION_V1.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V2.to_owned(),
            haider_rpc::FEATURE_CONVERGENCE_GRAPH_V3.to_owned(),
            FEATURE_HOOKS_V1.to_owned(),
            FEATURE_PROVIDER_CONFIGURE_V1.to_owned(),
            FEATURE_PROVIDER_MANAGEMENT_V1.to_owned(),
            FEATURE_PROVIDER_MODELS_V1.to_owned(),
            FEATURE_PROVIDER_REMOVE_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FAST_SELECT_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1.to_owned(),
            FEATURE_SESSION_MUTATION_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_RENAME_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_FLEET_V1.to_owned(),
            haider_rpc::FEATURE_SESSION_OBSERVE_V1.to_owned(),
            FEATURE_SESSION_PERMISSION_OVERRIDES_V1.to_owned(),
            FEATURE_SHELL_EXEC_V1.to_owned(),
            FEATURE_TOOL_INVENTORY_V1.to_owned(),
            haider_rpc::FEATURE_TRANSCRIPTION_V1.to_owned(),
            FEATURE_TURN_CONTROL_V1.to_owned(),
            haider_rpc::FEATURE_USAGE_REPORT_V1.to_owned(),
            FEATURE_VAULT_STAGE_V1.to_owned(),
        ])
    );
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

// MUTATION CHECK (R9 server read deadline): remove the liveness tick arm (or
// stop updating `last_read` on reads). Expected failure: this paused-time
// test never observes the close and times out at its outer bound / the
// elapsed window assertion fails.
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
