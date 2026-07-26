//! Golden JSON shape and serde tolerance tests.
#![allow(clippy::expect_used)]

mod common;

use std::fmt::Write as _;
use std::path::PathBuf;

use common::{TEST_FRAME_LIMIT, transcript};
use haider_rpc::{
    DEFAULT_FRAME_LIMIT, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_CAPABILITY_DENIED,
    ERROR_CODE_CURSOR_AHEAD, ERROR_CODE_DRAINING, ERROR_CODE_NOT_FOUND, Hello, RequestBody,
    ResponseBody, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec, ws_codec,
};
use serde::{Deserialize, Serialize};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("wire_transcript.json")
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GoldenWireBytes {
    ws_body: String,
    uds_stream_hex: String,
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut hex, "{byte:02x}").expect("write to String");
    }
    hex
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex must contain whole bytes");
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("valid fixture hex"))
        .collect()
}

#[test]
fn compact_ws_bodies_and_length_prefixed_uds_streams_are_golden() {
    let expected_frames = transcript();
    let expected_bytes: Vec<GoldenWireBytes> = expected_frames
        .iter()
        .map(|frame| {
            let ws_body = ws_codec::encode(frame, TEST_FRAME_LIMIT).expect("WS encode");
            let uds_stream =
                uds_codec::encode(frame, TEST_FRAME_LIMIT).expect("length-prefixed UDS encode");
            GoldenWireBytes {
                ws_body,
                uds_stream_hex: bytes_to_hex(&uds_stream),
            }
        })
        .collect();
    let serialized = serde_json::to_string_pretty(&expected_bytes).expect("serialize transcript");
    let path = fixture_path();
    if std::env::var("UPDATE_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("mkdir fixtures");
        std::fs::write(&path, &serialized).expect("write fixture");
    }
    let golden =
        std::fs::read_to_string(path).expect("missing wire fixture; run with UPDATE_FIXTURES=1");
    assert_eq!(serialized, golden);

    let pinned: Vec<GoldenWireBytes> = serde_json::from_str(&golden).expect("decode fixture");
    assert_eq!(pinned, expected_bytes);
    assert_eq!(pinned.len(), expected_frames.len());

    for (bytes, expected_frame) in pinned.into_iter().zip(expected_frames) {
        assert!(!bytes.ws_body.contains('\n'), "WS body must be compact");
        let ws_decoded =
            ws_codec::decode(&bytes.ws_body, TEST_FRAME_LIMIT).expect("decode pinned WS body");
        assert_eq!(ws_decoded, expected_frame);

        let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
        let batch = decoder.push(&hex_to_bytes(&bytes.uds_stream_hex));
        assert!(
            batch.error.is_none(),
            "decode pinned UDS: {:?}",
            batch.error
        );
        assert_eq!(batch.frames, vec![expected_frame]);
    }
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
fn additive_handshake_identity_fields_have_tolerant_decode_defaults() {
    let hello_json =
        r#"{"v":1,"kind":"hello","protocol_min":1,"protocol_max":1,"client_kind":"tui"}"#;
    let hello = serde_json::from_str::<WireFrame>(hello_json).expect("decode earlier hello");
    let WireFrame::Hello(Hello {
        client_name,
        client_version,
        client_instance_id,
        max_receive_frame,
        ..
    }) = hello
    else {
        panic!("expected hello");
    };
    assert!(client_name.is_empty());
    assert!(client_version.is_empty());
    assert!(client_instance_id.is_empty());
    assert_eq!(max_receive_frame as usize, DEFAULT_FRAME_LIMIT);

    let welcome_json = r#"{"v":1,"kind":"welcome","protocol":1,"instance_id":"daemon","daemon_generation":2,"frame_limit":1048576,"lifecycle_phase":"ready"}"#;
    let welcome = serde_json::from_str::<WireFrame>(welcome_json).expect("decode earlier welcome");
    let WireFrame::Welcome(Welcome {
        profile_id,
        daemon_version,
        ..
    }) = welcome
    else {
        panic!("expected welcome");
    };
    assert!(profile_id.is_empty());
    assert!(daemon_version.is_empty());
}

#[test]
fn correlated_errors_pin_the_named_stable_codes() {
    assert_eq!(
        [
            ERROR_CODE_CURSOR_AHEAD,
            ERROR_CODE_CAPABILITY_DENIED,
            ERROR_CODE_ALREADY_RESOLVED,
            ERROR_CODE_NOT_FOUND,
            ERROR_CODE_DRAINING,
        ],
        [
            "cursor_ahead",
            "capability_denied",
            "already_resolved",
            "not_found",
            "draining",
        ]
    );

    let frame = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error { .. },
                    ..
                }
            )
        })
        .expect("correlated error frame");
    let value = serde_json::to_value(frame).expect("error JSON");
    assert_eq!(value["kind"], "response");
    assert_eq!(value["request_id"], "request-control");
    assert_eq!(value["body"]["method"], "error");
    assert_eq!(
        value["body"]["code"],
        serde_json::Value::String(ERROR_CODE_CAPABILITY_DENIED.into())
    );
}

#[test]
fn cursor_pagination_and_lag_notice_have_no_numeric_resume_authority() {
    let frames = transcript();
    let list = frames
        .iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Request {
                    body: RequestBody::SessionList { .. },
                    ..
                }
            )
        })
        .expect("session list request");
    let list_value = serde_json::to_value(list).expect("list JSON");
    assert!(list_value["body"].get("cursor").is_some());
    assert!(list_value["body"].get("limit").is_some());
    assert!(list_value["body"].get("page").is_none());
    assert!(list_value["body"].get("page_size").is_none());

    let lagged = frames
        .iter()
        .find(|frame| matches!(frame, WireFrame::Lagged { .. }))
        .expect("lagged frame");
    let lagged_value = serde_json::to_value(lagged).expect("lagged JSON");
    assert_eq!(lagged_value["last_queued_seq"], 10);
    assert!(lagged_value.get("resume_after_seq").is_none());
}

#[test]
fn liveness_and_drain_metadata_are_top_level_and_timestamped() {
    let ping = serde_json::to_value(WireFrame::Ping { nonce: 17 }).expect("ping JSON");
    let pong = serde_json::to_value(WireFrame::Pong { nonce: 17 }).expect("pong JSON");
    assert_eq!(ping["kind"], "ping");
    assert_eq!(pong["kind"], "pong");
    assert!(ping.get("request_id").is_none());
    assert!(ping.get("body").is_none());

    let draining = transcript()
        .into_iter()
        .find(|frame| matches!(frame, WireFrame::ServerDraining { .. }))
        .expect("draining frame");
    let value = serde_json::to_value(draining).expect("draining JSON");
    assert_eq!(value["reason"], "upgrade");
    assert_eq!(value["instance_id"], "instance-1");
    assert_eq!(value["daemon_generation"], 4);
    assert_eq!(value["deadline_unix_ms"], 1_753_500_030_000_u64);
    assert!(value.get("deadline_ms").is_none());
}

#[test]
fn unsupported_wire_version_is_deliberately_strict() {
    let json = r#"{"v":2,"kind":"ping","nonce":42}"#;
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

    assert!(object.contains_key("attachment_id"));
    assert!(object.contains_key("envelope"));
    assert!(!object.contains_key("event_id"));
    assert!(!object.contains_key("notification_id"));
    assert!(!object.contains_key("snapshot_generation"));
}

#[test]
fn error_data_decodes_tolerantly_when_absent_or_unknown_kind() {
    // An old daemon's error frame carries no `data` key at all.
    let old_frame = r#"{"v":1,"kind":"response","request_id":"r-1","body":{"method":"error","code":"capability_denied","message":"nope","retryable":false}}"#;
    let decoded = ws_codec::decode(old_frame, DEFAULT_FRAME_LIMIT).expect("old error decodes");
    let WireFrame::Response {
        body: ResponseBody::Error { data, code, .. },
        ..
    } = decoded
    else {
        panic!("expected a correlated error response");
    };
    assert_eq!(code, ERROR_CODE_CAPABILITY_DENIED);
    assert!(data.is_none(), "absent data must decode as None");

    // A future daemon's data kind this crate does not know must decode as
    // ErrorData::Unknown, never fail the frame.
    let future_frame = r#"{"v":1,"kind":"response","request_id":"r-2","body":{"method":"error","code":"cursor_ahead","message":"ahead","retryable":true,"data":{"kind":"warp_offset","distance":9}}}"#;
    let decoded = ws_codec::decode(future_frame, DEFAULT_FRAME_LIMIT).expect("future data decodes");
    let WireFrame::Response {
        body: ResponseBody::Error { data, .. },
        ..
    } = decoded
    else {
        panic!("expected a correlated error response");
    };
    assert_eq!(data, Some(haider_rpc::ErrorData::Unknown));
}
