//! Golden JSON shape and serde tolerance tests.
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;

use common::transcript;
use haider_rpc::{RequestBody, WIRE_PROTOCOL_VERSION, WireFrame};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("wire_transcript.json")
}

#[test]
fn wire_transcript_json_shape_is_golden_and_round_trips() {
    let expected_frames = transcript();
    let serialized = serde_json::to_string_pretty(&expected_frames).expect("serialize transcript");
    let path = fixture_path();
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("mkdir fixtures");
        std::fs::write(&path, &serialized).expect("write fixture");
    }
    let golden =
        std::fs::read_to_string(path).expect("missing wire fixture; run with UPDATE_FIXTURES=1");
    assert_eq!(serialized, golden);

    let decoded: Vec<WireFrame> = serde_json::from_str(&golden).expect("decode fixture");
    assert_eq!(decoded, expected_frames);
}

#[test]
fn unknown_fields_and_future_method_discriminants_are_tolerated() {
    let json = format!(
        r#"{{
            "v": {WIRE_PROTOCOL_VERSION},
            "kind": "request",
            "request_id": "request-future",
            "body": {{"method": "session.teleport", "destination": "moon"}},
            "future_top_level": true
        }}"#
    );
    let decoded: WireFrame = serde_json::from_str(&json).expect("tolerant decode");
    assert_eq!(
        decoded,
        WireFrame::Request {
            request_id: haider_rpc::RequestId::new("request-future"),
            body: RequestBody::Unknown,
        }
    );
}

#[test]
fn unknown_top_level_frame_kind_is_tolerated() {
    let json = format!(r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"hologram","future":true}}"#);
    let decoded: WireFrame = serde_json::from_str(&json).expect("unknown kind");
    assert_eq!(decoded, WireFrame::Unknown);
}

#[test]
fn unsupported_wire_version_is_deliberately_strict() {
    let json = r#"{"v":2,"kind":"server_draining","deadline_ms":42}"#;
    let error = serde_json::from_str::<WireFrame>(json).expect_err("version must fail");
    assert!(error.to_string().contains("unsupported wire version 2"));
}

#[test]
fn event_frame_has_no_parallel_cursor_field() {
    let event = transcript()
        .into_iter()
        .find(|frame| matches!(frame, WireFrame::Event { .. }))
        .expect("event frame");
    let value = serde_json::to_value(event).expect("event JSON");
    let object = value.as_object().expect("event object");

    assert!(object.contains_key("envelope"));
    assert!(!object.contains_key("event_id"));
    assert!(!object.contains_key("notification_id"));
    assert!(!object.contains_key("snapshot_generation"));
}
