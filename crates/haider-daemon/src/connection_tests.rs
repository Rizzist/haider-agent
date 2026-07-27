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
        bytes: bytes.to_vec(),
        response_for: Some((attachment.clone(), RequestId::new(request))),
        floor: false,
    }
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
