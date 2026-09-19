#![allow(clippy::expect_used)]
//! Wire shapes for `session.attach` launch-origin registration
//! (`docs/design/dated-workspace-v1.md` §4–5: X1, O1).
//!
//! The exhaustive method pin stays 136 (`wire_golden_tests.rs`); these
//! tests pin the ADDITIVE bytes: absent fields are byte-identical to the
//! pre-feature frames, and the negotiated-present shapes round-trip.

use haider_protocol::ids::SessionId;
use haider_protocol::session::{
    LaunchOriginPathKindV1, LaunchOriginPathV1, LaunchOriginRegistrationV1, LaunchOriginV1,
};
use haider_rpc::{
    AttachMode, AttachState, AttachmentId, FEATURE_SESSION_LAUNCH_ORIGIN_V1, RequestBody,
    RequestId, ResponseBody, WireFrame, uds_codec,
};

const LIMIT: usize = 1024 * 1024;

fn request(body: RequestBody) -> WireFrame {
    WireFrame::Request {
        request_id: RequestId::new("request-origin"),
        body,
    }
}

fn response(body: ResponseBody) -> WireFrame {
    WireFrame::Response {
        request_id: RequestId::new("request-origin"),
        body,
    }
}

fn encode(frame: &WireFrame) -> Vec<u8> {
    uds_codec::encode(frame, LIMIT).expect("encode frame")
}

fn decode(bytes: &[u8]) -> WireFrame {
    let mut decoder = uds_codec::Decoder::new(LIMIT);
    let batch = decoder.push(bytes);
    assert!(batch.error.is_none(), "decode error: {:?}", batch.error);
    assert_eq!(batch.frames.len(), 1);
    batch.frames.into_iter().next().expect("one frame")
}

fn registration() -> LaunchOriginRegistrationV1 {
    LaunchOriginRegistrationV1 {
        command_id: "origin-open-1-session-1".into(),
        open_id: "open-1".into(),
        worker_generation: 7,
        expected_revision: 0,
        path: LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::HomeRelative,
            display: Some("~/Documents".into()),
        },
        workspace_materialized: Some(false),
    }
}

fn snapshot() -> LaunchOriginV1 {
    LaunchOriginV1 {
        subject_session_id: "session-1".into(),
        open_id: "open-1".into(),
        revision: 1,
        path: LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::HomeRelative,
            display: Some("~/Documents".into()),
        },
        recorded_at_ms: 1_000,
        selected_seq: 5,
    }
}

/// X1: the encode-only legacy variant and the decode-normal form with an
/// absent registration produce byte-identical `session.attach` frames —
/// the exact pre-feature bytes.
#[test]
fn absent_registration_keeps_legacy_attach_bytes() {
    let legacy = encode(&request(RequestBody::SessionAttach {
        session_id: SessionId::new("session-1"),
        after_seq: 4,
        mode: AttachMode::Control,
        sealed_replay: false,
    }));
    let absent = encode(&request(RequestBody::SessionAttachWithOrigin {
        session_id: SessionId::new("session-1"),
        after_seq: 4,
        mode: AttachMode::Control,
        sealed_replay: false,
        launch_origin: None,
    }));
    assert_eq!(
        legacy, absent,
        "absent-field bytes must be pre-feature bytes"
    );
    let body = String::from_utf8(legacy[4..].to_vec()).expect("utf8 frame");
    assert!(
        !body.contains("launch_origin"),
        "no vestigial field: {body}"
    );
}

/// A pre-feature attach decodes into the normal form with no registration.
#[test]
fn legacy_attach_json_decodes_with_absent_origin() {
    let bytes = encode(&request(RequestBody::SessionAttach {
        session_id: SessionId::new("session-1"),
        after_seq: 4,
        mode: AttachMode::View,
        sealed_replay: true,
    }));
    let WireFrame::Request { body, .. } = decode(&bytes) else {
        panic!("expected request frame");
    };
    assert_eq!(
        body,
        RequestBody::SessionAttachWithOrigin {
            session_id: SessionId::new("session-1"),
            after_seq: 4,
            mode: AttachMode::View,
            sealed_replay: true,
            launch_origin: None,
        }
    );
}

/// O1: the negotiated-present registration round-trips exactly.
#[test]
fn present_registration_round_trips() {
    let frame = request(RequestBody::SessionAttachWithOrigin {
        session_id: SessionId::new("session-1"),
        after_seq: 0,
        mode: AttachMode::Control,
        sealed_replay: false,
        launch_origin: Some(registration()),
    });
    let bytes = encode(&frame);
    assert_eq!(decode(&bytes), frame);
    let body = String::from_utf8(bytes[4..].to_vec()).expect("utf8 frame");
    assert!(body.contains("\"method\":\"session.attach\""));
    assert!(body.contains("\"kind\":\"home_relative\""));
    assert!(body.contains("\"expected_revision\":0"));
}

/// X1: an attach response without a snapshot keeps pre-feature bytes; a
/// present snapshot round-trips.
#[test]
fn response_snapshot_is_additive() {
    let state = AttachState {
        session_id: SessionId::new("session-1"),
        requested_after_seq: 0,
        replay_through_seq: 4,
        worker_generation: 7,
        authority_epoch: 1,
    };
    let absent = encode(&response(ResponseBody::SessionAttach {
        attachment_id: AttachmentId::new("att-1"),
        attach_state: state.clone(),
        launch_origin: None,
    }));
    let body = String::from_utf8(absent[4..].to_vec()).expect("utf8 frame");
    assert!(
        !body.contains("launch_origin"),
        "absent snapshot must stay off the wire: {body}"
    );

    let frame = response(ResponseBody::SessionAttach {
        attachment_id: AttachmentId::new("att-1"),
        attach_state: state,
        launch_origin: Some(snapshot()),
    });
    assert_eq!(decode(&encode(&frame)), frame);
}

/// Unknown origin path kinds must not be silently reinterpreted: the typed
/// decode refuses them (callers keep the raw envelope law for events).
#[test]
fn unknown_path_kind_is_refused_by_typed_decode() {
    let result = serde_json::from_value::<LaunchOriginPathV1>(serde_json::json!({
        "kind": "teleported",
        "display": "/opt"
    }));
    assert!(result.is_err());
}

#[test]
fn feature_token_is_pinned() {
    assert_eq!(FEATURE_SESSION_LAUNCH_ORIGIN_V1, "session_launch_origin_v1");
}
