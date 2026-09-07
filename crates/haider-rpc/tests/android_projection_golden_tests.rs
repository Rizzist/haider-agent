#![allow(clippy::expect_used)]
//! Android consumes these Rust-produced presentation payloads directly as test resources.
//! Regenerate with UPDATE_ANDROID_GOLDEN=1 cargo test -p haider-rpc --test android_projection_golden_tests.

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
