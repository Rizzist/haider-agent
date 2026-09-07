#![allow(clippy::expect_used)]
//! Android consumes these Rust-produced presentation payloads directly as test resources.
//! Regenerate with UPDATE_ANDROID_GOLDEN=1 cargo test -p haider-rpc --test android_projection_golden_tests.

use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::{DeliveryMode, EventPayload};

#[test]
fn android_display_payloads_match_rust_serialization() {
    let payloads = vec![
        EventPayload::UserMessage {
            text: "Find the old conversation".into(),
            attachments: vec![],
            mode: DeliveryMode::Queue,
        },
        EventPayload::Item(ItemEvent::Started {
            item_id: haider_protocol::ids::ItemId::new("answer-1"),
            item: TurnItem::AgentMessage { text: "".into() },
        }),
        EventPayload::Item(ItemEvent::Delta {
            item_id: haider_protocol::ids::ItemId::new("answer-1"),
            delta: ItemDelta::Text {
                text: "Found ".into(),
            },
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("answer-1"),
            item: TurnItem::AgentMessage {
                text: "Found your conversation".into(),
            },
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id: haider_protocol::ids::ItemId::new("plan-1"),
            item: TurnItem::Plan {
                items: vec![TodoItem {
                    id: 1,
                    text: "Visible plan must not vanish from coverage".into(),
                    state: TodoState::Listed,
                    dep: None,
                }],
            },
        }),
    ];
    let value =
        serde_json::to_string_pretty(&payloads).expect("serialize actual Rust payloads") + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/android_display_payloads_v1.json");
    if std::env::var_os("UPDATE_ANDROID_GOLDEN").is_some() {
        std::fs::write(&path, &value).expect("write Android fixture");
    }
    assert_eq!(
        std::fs::read_to_string(path).expect("read Android fixture"),
        value
    );
}

#[test]
fn android_liveness_matches_rust_wire_and_uds_bytes() {
    let frames = [
        haider_rpc::WireFrame::Ping { nonce: 17 },
        haider_rpc::WireFrame::Pong { nonce: 17 },
        haider_rpc::WireFrame::Ping { nonce: u64::MAX },
        haider_rpc::WireFrame::Pong { nonce: u64::MAX },
    ];
    let rows: Vec<_> = frames.iter().map(|frame| {
        let body = serde_json::to_string(frame).expect("serialize liveness");
        let bytes = haider_rpc::uds_codec::encode(frame, 1024).expect("encode liveness");
        serde_json::json!({"ws_body": body, "uds_stream_hex": bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
    }).collect();
    let actual = serde_json::to_string_pretty(&rows).expect("serialize fixture") + "\n";
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/android_liveness_wire_v1.json");
    if std::env::var_os("UPDATE_ANDROID_GOLDEN").is_some() {
        std::fs::write(&path, &actual).expect("write liveness fixture");
    }
    assert_eq!(
        std::fs::read_to_string(path).expect("read liveness fixture"),
        actual
    );
}
