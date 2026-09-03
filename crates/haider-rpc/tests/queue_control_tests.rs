#![allow(clippy::expect_used)]

use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::queue::{QueueChange, QueueDelta, QueueRow};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_rpc::{AttachmentId, FEATURE_QUEUE_CONTROL_V1, WireFrame};

#[test]
fn queue_feature_and_revision_bearing_delta_are_golden() {
    assert_eq!(FEATURE_QUEUE_CONTROL_V1, "queue_control_v1");
    let row = QueueRow {
        id: EventId::new("user-queued-1"),
        text: "  keep this text\nverbatim  ".into(),
        mode: DeliveryMode::Queue,
        ordinal: 1,
        created_at_ms: 1_753_500_000_000,
    };
    let payload = EventPayload::QueueChanged(QueueDelta {
        revision: 31,
        change: QueueChange::Enqueued { row },
    });
    assert_eq!(
        serde_json::to_value(&payload).expect("encode queue delta"),
        serde_json::json!({
            "type": "queue_changed",
            "revision": 31,
            "change": {
                "kind": "enqueued",
                "row": {
                    "id": "user-queued-1",
                    "text": "  keep this text\nverbatim  ",
                    "mode": "queue",
                    "ordinal": 1,
                    "created_at_ms": 1_753_500_000_000_u64,
                }
            }
        })
    );

    let frame = WireFrame::Event {
        attachment_id: AttachmentId::new("attachment-1"),
        session_id: SessionId::new("session-1"),
        envelope: EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("queue-delta-1"),
            seq: 31,
            session_id: SessionId::new("session-1"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("daemon-1"),
            authority_epoch: 1,
            worker_generation: 3,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 1_753_500_000_000,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("payload").into(),
        },
    };
    let encoded = serde_json::to_vec(&frame).expect("encode event frame");
    assert_eq!(
        serde_json::from_slice::<WireFrame>(&encoded).expect("decode event frame"),
        frame
    );
}

#[test]
fn queue_delta_cannot_decode_without_its_revision() {
    let error = serde_json::from_value::<EventPayload>(serde_json::json!({
        "type": "queue_changed",
        "change": {"kind": "removed", "id": "user-queued-1"}
    }))
    .expect_err("revision is structurally required");
    assert!(error.to_string().contains("revision"));
}
