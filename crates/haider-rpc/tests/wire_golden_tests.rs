//! Golden JSON shape and serde tolerance tests.
#![allow(clippy::expect_used)]

mod common;

use std::fmt::Write as _;
use std::path::PathBuf;

use common::{TEST_FRAME_LIMIT, transcript};
use haider_rpc::{
    CancelStatus, DEFAULT_FRAME_LIMIT, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_BUSY,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CREDENTIAL_MISSING, ERROR_CODE_CURSOR_AHEAD,
    ERROR_CODE_DRAINING, ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_INVALID_CURSOR,
    ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED, ERROR_CODE_PERMISSION_DENIED,
    ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_RESTAGE_REQUIRED, ERROR_CODE_REVISION_CONFLICT,
    ERROR_CODE_RUN_NOT_ACTIVE, ERROR_CODE_STALE_GENERATION, ERROR_CODE_UNAUTHORIZED,
    ERROR_CODE_VAULT_UNSUPPORTED, FEATURE_ACCOUNT_LOGIN_API_V1, FEATURE_ACCOUNT_MANAGEMENT_V1,
    FEATURE_ACCOUNT_OAUTH_PKCE_V1, FEATURE_ACCOUNT_ROTATION_V1, FEATURE_PROVIDER_CONFIGURE_V1,
    FEATURE_PROVIDER_MANAGEMENT_V1, FEATURE_PROVIDER_MODELS_V1, FEATURE_SESSION_MUTATION_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_VAULT_STAGE_V1, Hello, RequestBody, ResponseBody,
    SubmitDisposition, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec, ws_codec,
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

/// MUTATION CHECK: remove either appended W8a shell/inventory frame from the
/// transcript. Expected runtime failure: the compact WS and UDS golden byte
/// arrays differ in length/content while every pre-W8 frame stays unchanged.
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
        features,
        ..
    }) = welcome
    else {
        panic!("expected welcome");
    };
    assert!(profile_id.is_empty());
    assert!(daemon_version.is_empty());
    assert!(features.is_empty());
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
            ERROR_CODE_OVERLOADED,
            ERROR_CODE_INVALID_CURSOR,
            ERROR_CODE_INVALID_ARGUMENT,
            ERROR_CODE_STALE_GENERATION,
            ERROR_CODE_RUN_NOT_ACTIVE,
            ERROR_CODE_BUSY,
            ERROR_CODE_PROVIDER_ERROR,
        ],
        [
            "cursor_ahead",
            "capability_denied",
            "already_resolved",
            "not_found",
            "draining",
            "overloaded",
            "invalid_cursor",
            "invalid_argument",
            "stale_generation",
            "run_not_active",
            "busy",
            "provider_error",
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

/// The five W3c2 account/vault codes pinned as WIRE LITERALS: a client
/// matching on `"restage_required"` must keep matching, so the constant's
/// value and the encoded frame's bytes are both asserted against the string
/// (asserting the constant against itself would pin nothing).
///
/// MUTATION CHECK: change any of the five constants' values in `frame.rs`
/// (e.g. `ERROR_CODE_RESTAGE_REQUIRED` to `"restage_needed"`). Expected
/// failure: the literal array below mismatches AND the encoded WS body no
/// longer contains `"code":"restage_required"`.
#[test]
fn account_and_vault_stable_codes_pin_their_wire_literals() {
    assert_eq!(
        [
            ERROR_CODE_UNAUTHORIZED,
            ERROR_CODE_PERMISSION_DENIED,
            ERROR_CODE_RESTAGE_REQUIRED,
            ERROR_CODE_VAULT_UNSUPPORTED,
            ERROR_CODE_CREDENTIAL_MISSING,
        ],
        [
            "unauthorized",
            "permission_denied",
            "restage_required",
            "vault_unsupported",
            "credential_missing",
        ]
    );

    // Each code also travels as that literal inside a correlated error frame
    // (and an older client carries the bytes back unchanged).
    for (code, literal, retryable) in [
        (ERROR_CODE_UNAUTHORIZED, "unauthorized", false),
        (ERROR_CODE_PERMISSION_DENIED, "permission_denied", false),
        (ERROR_CODE_RESTAGE_REQUIRED, "restage_required", true),
        (ERROR_CODE_VAULT_UNSUPPORTED, "vault_unsupported", false),
        (ERROR_CODE_CREDENTIAL_MISSING, "credential_missing", false),
    ] {
        let frame = WireFrame::Response {
            request_id: haider_rpc::RequestId::new("request-login"),
            body: ResponseBody::Error {
                code: code.into(),
                message: "pinned code".into(),
                retryable,
                data: None,
            },
        };
        let ws_body = ws_codec::encode(&frame, TEST_FRAME_LIMIT).expect("WS encode");
        assert!(
            ws_body.contains(&format!(r#""code":"{literal}""#)),
            "wire body must carry the literal code {literal}: {ws_body}"
        );
        let value = serde_json::to_value(&frame).expect("error JSON");
        assert_eq!(value["body"]["method"], "error");
        assert_eq!(
            value["body"]["code"],
            serde_json::Value::String(literal.into())
        );
        assert_eq!(
            value["body"]["retryable"],
            serde_json::Value::Bool(retryable)
        );
        assert_eq!(
            ws_codec::decode(&ws_body, TEST_FRAME_LIMIT).expect("decode pinned error"),
            frame
        );
    }
}

/// MUTATION CHECK: remove `Welcome.features`' default/skip-empty attributes.
/// Expected failure: the old Welcome golden changes bytes or the legacy
/// no-features frame stops decoding.
#[test]
fn method_features_are_additive_sorted_and_absent_when_empty() {
    let frames = transcript();
    let old = serde_json::to_value(&frames[1]).expect("old welcome");
    assert!(old.get("features").is_none());

    let featured = frames
        .iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Welcome(Welcome { features, .. }) if !features.is_empty()
            )
        })
        .expect("featured welcome");
    let value = serde_json::to_value(featured).expect("featured welcome JSON");
    assert_eq!(
        value["features"],
        serde_json::json!([FEATURE_SESSION_MUTATION_V1, FEATURE_TURN_CONTROL_V1])
    );
}

/// MUTATION CHECK: remove a new method's explicit rename/tag or its
/// `#[serde(other)]` status fallback. Expected failure: the exact v1 method
/// name changes, or a future status no longer decodes to `Unknown`.
#[test]
fn new_methods_are_kind_tagged_and_new_statuses_are_unknown_tolerant() {
    let frames = transcript();
    for method in ["session.create", "turn.submit", "turn.cancel"] {
        assert!(
            frames.iter().any(|frame| {
                serde_json::to_value(frame)
                    .ok()
                    .is_some_and(|value| value["body"]["method"] == method)
            }),
            "missing golden pair for {method}"
        );
    }

    let submit: SubmitDisposition =
        serde_json::from_str(r#""future_submit_state""#).expect("unknown submit status");
    let cancel: CancelStatus =
        serde_json::from_str(r#""future_cancel_state""#).expect("unknown cancel status");
    assert_eq!(submit, SubmitDisposition::Unknown);
    assert_eq!(cancel, CancelStatus::Unknown);
}

/// A new reader ignores additive fields on known W3c methods. An old reader's
/// open method enum is covered by `unknown_fields_and_future_method_discriminants_are_tolerated`.
#[test]
fn session_create_ignores_unknown_additive_fields() {
    let json = r#"{
        "method":"session.create",
        "command_id":"create-1",
        "cwd":"/tmp",
        "provider":"fake",
        "model":"fake-v1",
        "max_tokens":4096,
        "future_policy":{"mode":"strict"}
    }"#;
    let body: RequestBody = serde_json::from_str(json).expect("known method with additive field");
    assert!(matches!(body, RequestBody::SessionCreate { .. }));
}

/// R7 additive-field tolerance for the two turn mutation methods
/// (`session_create_ignores_unknown_additive_fields` is the twin).
///
/// MUTATION CHECK: make `RequestBody` decoding strict about unknown fields
/// (e.g. `#[serde(deny_unknown_fields)]`). Expected failure: both decodes
/// below reject the additive field.
#[test]
fn turn_submit_and_cancel_ignore_unknown_additive_fields() {
    let submit = r#"{
        "method":"turn.submit",
        "command_id":"submit-1",
        "session_id":"session-1",
        "worker_generation":1,
        "text":"hello",
        "attachments":[],
        "mode":"queue",
        "future_priority":"high"
    }"#;
    let body: RequestBody = serde_json::from_str(submit).expect("submit with additive field");
    assert!(matches!(body, RequestBody::TurnSubmit { .. }));
    let cancel = r#"{
        "method":"turn.cancel",
        "command_id":"cancel-1",
        "session_id":"session-1",
        "worker_generation":1,
        "run_id":"run-1",
        "future_reason":"user"
    }"#;
    let body: RequestBody = serde_json::from_str(cancel).expect("cancel with additive field");
    assert!(matches!(body, RequestBody::TurnCancel { .. }));
}

/// MUTATION CHECK: remove the additive `ResponseBody::MenuAnswer` variant or
/// rename its method/coordinate. Expected failure: the exact success shape
/// below no longer round-trips.
#[test]
fn menu_answer_success_is_correlated_by_request_and_resolution_sequence() {
    let frame = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("menu-success"),
        body: ResponseBody::MenuAnswer { resolution_seq: 42 },
    };
    let json = serde_json::to_value(&frame).expect("encode success");
    assert_eq!(json["body"]["method"], "menu.answer");
    assert_eq!(json["body"]["resolution_seq"], 42);
    assert_eq!(
        serde_json::from_value::<WireFrame>(json).expect("decode success"),
        frame
    );
}

/// `MenuAnswer.request_id` is purely additive correlation: it may be absent
/// (and then leaves no trace on the wire), and it never displaces
/// `command_id`, which stays the durable compare-and-set identity.
#[test]
fn menu_answer_request_id_is_optional_correlation_beside_the_durable_command_id() {
    let json = format!(
        r#"{{
            "v": {WIRE_PROTOCOL_VERSION},
            "kind": "menu_answer",
            "command_id": "command-legacy",
            "session_id": "session-1",
            "menu_id": "menu-1",
            "request_seq": 3,
            "worker_generation": 7,
            "option_key": "approve",
            "option_index": 0
        }}"#
    );
    let decoded: WireFrame = serde_json::from_str(&json).expect("decode uncorrelated menu answer");
    let WireFrame::MenuAnswer {
        request_id,
        command_id,
        ..
    } = &decoded
    else {
        panic!("expected menu answer, got {decoded:?}");
    };
    assert!(request_id.is_none(), "the field must stay optional");
    assert_eq!(command_id.as_str(), "command-legacy");
    let reserialized = serde_json::to_value(&decoded).expect("re-encode");
    assert!(
        reserialized.get("request_id").is_none(),
        "an absent correlation must not appear on the wire"
    );
    assert_eq!(reserialized["command_id"], "command-legacy");

    let correlated = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::MenuAnswer {
                    request_id: Some(_),
                    ..
                }
            )
        })
        .expect("correlated menu answer");
    let value = serde_json::to_value(&correlated).expect("correlated JSON");
    assert_eq!(value["request_id"], "request-menu-1");
    assert_eq!(value["command_id"], "command-1");
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

/// MUTATION CHECK: replace `SecretWire`'s manual redacted `Debug` with a
/// derived one (or format the raw value). Expected failure: the placeholder
/// leaks into the formatted frame and both assertions below fail.
#[test]
fn stage_frame_debug_formatting_never_reveals_the_secret() {
    let frame = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-stage"),
        body: RequestBody::VaultStage {
            stage_id: "stage-1".into(),
            purpose: haider_rpc::StagePurpose::ApiKey,
            secret: haider_rpc::SecretWire::new("sk-debug-sentinel-1f2e3d4c"),
        },
    };
    let debug = format!("{frame:?}");
    assert!(
        !debug.contains("sk-debug-sentinel-1f2e3d4c"),
        "ordinary frame formatting must never reveal a staged secret: {debug}"
    );
    assert!(
        debug.contains("[REDACTED]"),
        "redaction marker missing: {debug}"
    );
}

/// The W3c2 account/vault methods obey the same additive tolerance rules as
/// every v1 method: unknown fields ignored, kind-tagged method names exact,
/// and the four W3c feature strings golden in sorted order.
///
/// MUTATION CHECK: add `#[serde(deny_unknown_fields)]` to `RequestBody` or
/// rename a `vault.stage`/`account.*` tag. Expected failure: the tolerant
/// decodes below error, or the tag round-trip changes.
#[test]
fn account_methods_are_kind_tagged_and_unknown_field_tolerant() {
    for method in ["vault.stage", "account.login_api", "account.list"] {
        let found = transcript().into_iter().any(|frame| {
            matches!(
                &frame,
                WireFrame::Request { body, .. }
                    if serde_json::to_value(body).expect("body json")["method"] == method
            )
        });
        assert!(found, "transcript must pin request method {method}");
    }
    let login_json = format!(
        r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"request","request_id":"r1","body":{{
            "method":"account.login_api","command_id":"c1","provider":"anthropic",
            "vault_reference":"vaultref-1","future_login_field":true}}}}"#
    );
    let decoded: WireFrame = serde_json::from_str(&login_json).expect("tolerant login decode");
    match decoded {
        WireFrame::Request {
            body:
                RequestBody::AccountLoginApi {
                    alias,
                    validation_model,
                    ..
                },
            ..
        } => {
            // Optional additive fields default to None for older writers.
            assert_eq!(alias, None);
            assert_eq!(validation_model, None);
        }
        other => panic!("expected AccountLoginApi, got {other:?}"),
    }
    let stage_json = format!(
        r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"request","request_id":"r2","body":{{
            "method":"vault.stage","stage_id":"s1","purpose":"quantum_key","secret":"x"}}}}"#
    );
    let decoded: WireFrame = serde_json::from_str(&stage_json).expect("tolerant stage decode");
    match decoded {
        WireFrame::Request {
            body: RequestBody::VaultStage { purpose, .. },
            ..
        } => assert_eq!(purpose, haider_rpc::StagePurpose::Unknown),
        other => panic!("expected VaultStage, got {other:?}"),
    }
    // The four W3c feature families are golden, sorted, additive.
    let featured = transcript()
        .into_iter()
        .filter_map(|frame| match frame {
            WireFrame::Welcome(welcome) if welcome.features.len() == 4 => Some(welcome),
            _ => None,
        })
        .next()
        .expect("four-feature welcome");
    let value = serde_json::to_value(&featured).expect("welcome json");
    assert_eq!(
        value["features"],
        serde_json::json!([
            haider_rpc::FEATURE_ACCOUNT_LOGIN_API_V1,
            haider_rpc::FEATURE_SESSION_MUTATION_V1,
            haider_rpc::FEATURE_TURN_CONTROL_V1,
            haider_rpc::FEATURE_VAULT_STAGE_V1,
        ])
    );
}

/// An old daemon that predates the account surface answers the new methods
/// as `Unknown` (never a panic), and an old CLIENT tolerates the new
/// response methods the same way — the additive-wire law in both directions.
#[test]
fn account_methods_decode_as_unknown_for_older_readers() {
    // A pre-W3c2 reader is simulated by an arbitrary future method name:
    // the open enum treats every unimplemented method identically.
    let future = format!(
        r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"response","request_id":"r9","body":{{"method":"account.future_login_v9","x":1}}}}"#
    );
    let decoded: WireFrame = serde_json::from_str(&future).expect("tolerant response decode");
    assert_eq!(
        decoded,
        WireFrame::Response {
            request_id: haider_rpc::RequestId::new("r9"),
            body: ResponseBody::Unknown,
        }
    );
}

/// MUTATION CHECK: deriving `Debug` for either sensitive OAuth wire type
/// leaks the state-bearing URL or ready reference.
#[test]
fn oauth_authorization_and_ready_ref_codec_round_trip_but_debug_redacts() {
    const URL_SENTINEL: &str = "STATE_WIRE_SENTINEL_7fa13d";
    const READY_SENTINEL: &str = "READY_REF_SENTINEL_4b9c21";
    let frame = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("oauth-start"),
        body: ResponseBody::AccountOAuthStart {
            availability: haider_rpc::OAuthAvailabilityWire {
                available: true,
                reason: None,
            },
            flow_id: Some(haider_rpc::OAuthFlowId::new("flow-1")),
            authorization_url: Some(haider_rpc::OAuthAuthorizationWire::new(format!(
                "https://auth.example.invalid/authorize?state={URL_SENTINEL}&redirect_uri=http%3A%2F%2F127.0.0.1%3A49152%2Fcallback"
            ))),
            provider_origin: Some("https://auth.example.invalid".into()),
            loopback_port: Some(49_152),
            expires_at_ms: Some(99),
        },
    };
    let encoded = uds_codec::encode_zeroizing(&frame, DEFAULT_FRAME_LIMIT).expect("encode");
    let mut decoder = uds_codec::Decoder::new_zeroizing(DEFAULT_FRAME_LIMIT);
    let batch = decoder.push(&encoded);
    assert!(batch.error.is_none());
    assert_eq!(batch.frames, vec![frame.clone()]);
    let debug = format!("{frame:?}");
    assert!(!debug.contains(URL_SENTINEL));
    assert!(debug.contains("provider_origin: \"https://auth.example.invalid\""));
    assert!(debug.contains("loopback_port: Some(49152)"));

    let ready = haider_rpc::OAuthReadyRefWire::new(READY_SENTINEL);
    assert!(!format!("{ready:?}").contains(READY_SENTINEL));
    assert_eq!(
        serde_json::to_string(&ready).expect("ready encode"),
        format!("\"{READY_SENTINEL}\"")
    );
}

/// MUTATION CHECK: removing one explicit OAuth method arm/tag, making an
/// additive field mandatory, or removing the unknown status fallback kills
/// this test.
#[test]
fn oauth_methods_features_and_status_are_additive_unknown_tolerant() {
    for method in [
        "account.oauth_start",
        "account.oauth_status",
        "account.oauth_cancel",
        "account.add",
    ] {
        assert!(
            transcript().into_iter().any(|frame| {
                matches!(
                    frame,
                    WireFrame::Request { body, .. }
                        if serde_json::to_value(&body).expect("json")["method"] == method
                )
            }),
            "missing OAuth request golden for {method}"
        );
    }
    let future_status: haider_rpc::OAuthFlowStatusWire =
        serde_json::from_str(r#"{"status":"future_browser_phase","extra":true}"#)
            .expect("unknown status");
    assert_eq!(future_status, haider_rpc::OAuthFlowStatusWire::Unknown);

    let start: RequestBody = serde_json::from_str(
        r#"{"method":"account.oauth_start","provider":"fake","desired_alias":"work","attempt_id":"a","future":1}"#,
    )
    .expect("additive start");
    assert!(matches!(start, RequestBody::AccountOAuthStart { .. }));

    let welcome = transcript()
        .into_iter()
        .find_map(|frame| match frame {
            WireFrame::Welcome(welcome) if welcome.features.len() == 6 => Some(welcome),
            _ => None,
        })
        .expect("OAuth featured welcome");
    assert_eq!(
        serde_json::to_value(welcome).expect("json")["features"],
        serde_json::json!([
            haider_rpc::FEATURE_ACCOUNT_LOGIN_API_V1,
            haider_rpc::FEATURE_ACCOUNT_MANAGEMENT_V1,
            haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1,
            haider_rpc::FEATURE_SESSION_MUTATION_V1,
            haider_rpc::FEATURE_TURN_CONTROL_V1,
            haider_rpc::FEATURE_VAULT_STAGE_V1,
        ])
    );
}

#[test]
fn oauth_status_shapes_cannot_carry_callback_or_token_secrets() {
    for status in [
        haider_rpc::OAuthFlowStatusWire::WaitingBrowser,
        haider_rpc::OAuthFlowStatusWire::Exchanging,
        haider_rpc::OAuthFlowStatusWire::Failed {
            public_code: "access_denied".into(),
        },
        haider_rpc::OAuthFlowStatusWire::Expired,
        haider_rpc::OAuthFlowStatusWire::Cancelled,
    ] {
        let json = serde_json::to_string(&status).expect("status json");
        for forbidden in [
            "authorization_code",
            "code_verifier",
            "access_token",
            "refresh_token",
            "id_token",
            "authorization_url",
        ] {
            assert!(!json.contains(forbidden));
        }
    }
}

/// W5c.2a's additive account read and tolerant provider vocabulary.
///
/// MUTATION CHECK: remove `#[serde(other)]` from
/// `ProviderApiFamilyWire::Unknown`. Expected runtime failure: decoding
/// `"future_native_family"` below returns an error instead of `Unknown`.
#[test]
fn management_reads_preserve_legacy_account_list_and_tolerate_future_providers() {
    let legacy: ResponseBody =
        serde_json::from_str(r#"{"method":"account.list","descriptors":[]}"#)
            .expect("legacy account.list");
    assert_eq!(
        legacy,
        ResponseBody::AccountList {
            descriptors: Vec::new(),
            revision: None,
            provider_active: Vec::new(),
            provider_defaults: Vec::new(),
        }
    );
    assert_eq!(
        serde_json::to_value(&legacy).expect("legacy re-encode"),
        serde_json::json!({"method":"account.list","descriptors":[]})
    );

    let managed = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::AccountList {
                        revision: Some(7),
                        ..
                    },
                    ..
                }
            )
        })
        .expect("managed account.list golden");
    let value = serde_json::to_value(managed).expect("managed account JSON");
    assert_eq!(value["body"]["revision"], 7);
    assert_eq!(
        value["body"]["provider_active"],
        serde_json::json!([{
            "provider": "anthropic",
            "alias": "anthropic-0123456789abcdef01234567"
        }])
    );
    assert_eq!(
        value["body"]["provider_defaults"],
        serde_json::json!([{"provider": "anthropic", "model": "frontier-anthropic"}])
    );

    let family: haider_rpc::ProviderApiFamilyWire =
        serde_json::from_str(r#""future_native_family""#).expect("future family");
    let availability: haider_rpc::ProviderAvailabilityWire =
        serde_json::from_str(r#""degraded_by_moon_phase""#).expect("future availability");
    assert_eq!(family, haider_rpc::ProviderApiFamilyWire::Unknown);
    assert_eq!(availability, haider_rpc::ProviderAvailabilityWire::Unknown);
}

/// The account enums remain the original closed v1 vocabulary.
///
/// MUTATION CHECK: add `#[serde(alias = "passkey")]` to
/// `AuthMethod::ApiKey`. Expected runtime failure: the first `is_err`
/// assertion becomes false because the forbidden value decodes as an API key.
#[test]
fn account_auth_method_and_credential_status_remain_closed() {
    assert!(
        serde_json::from_str::<haider_protocol::credential::AuthMethod>(r#""passkey""#).is_err()
    );
    assert!(
        serde_json::from_str::<haider_protocol::credential::CredentialStatus>(
            r#"{"status":"future_health"}"#
        )
        .is_err()
    );
}

/// Stable optimistic-concurrency error bytes, including bounded coordinates.
///
/// MUTATION CHECK: change `ERROR_CODE_REVISION_CONFLICT` to
/// `"revision_changed"`. Expected runtime failure: the stable literal and
/// exact response-body assertions mismatch.
#[test]
fn revision_conflict_code_and_structured_body_are_golden() {
    assert_eq!(ERROR_CODE_REVISION_CONFLICT, "revision_conflict");
    let frame = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error { code, .. },
                    ..
                } if code == ERROR_CODE_REVISION_CONFLICT
            )
        })
        .expect("revision conflict golden");
    let value = serde_json::to_value(frame).expect("revision conflict JSON");
    assert_eq!(
        value["body"],
        serde_json::json!({
            "method": "error",
            "code": "revision_conflict",
            "message": "management snapshot changed",
            "retryable": true,
            "data": {
                "kind": "revision_conflict",
                "expected_revision": 6,
                "current_revision": 7
            }
        })
    );
}

/// The provider read/mutation methods and all honestly served feature
/// families.
///
/// MUTATION CHECK: remove `FEATURE_PROVIDER_MODELS_V1` from the final Welcome.
/// Expected runtime failure: the exact ten-feature assertion below omits
/// `provider_models_v1`.
/// Verified by revert on 2026-07-30.
#[test]
fn provider_list_and_management_feature_families_are_golden() {
    for method in [
        "provider.list",
        "account.set_active",
        "account.remove",
        "account.set_default_model",
        "provider.configure",
        "provider.models_refresh",
    ] {
        for kind in ["request", "response"] {
            assert!(transcript().into_iter().any(|frame| {
                let value = serde_json::to_value(frame).expect("frame JSON");
                value["kind"] == kind && value["body"]["method"] == method
            }));
        }
    }
    let welcome = transcript()
        .into_iter()
        .find_map(|frame| match frame {
            WireFrame::Welcome(welcome) if welcome.features.len() == 10 => Some(welcome),
            _ => None,
        })
        .expect("management-feature Welcome");
    assert_eq!(
        serde_json::to_value(welcome).expect("Welcome JSON")["features"],
        serde_json::json!([
            FEATURE_ACCOUNT_LOGIN_API_V1,
            FEATURE_ACCOUNT_MANAGEMENT_V1,
            FEATURE_ACCOUNT_OAUTH_PKCE_V1,
            FEATURE_ACCOUNT_ROTATION_V1,
            FEATURE_PROVIDER_CONFIGURE_V1,
            FEATURE_PROVIDER_MANAGEMENT_V1,
            FEATURE_PROVIDER_MODELS_V1,
            FEATURE_SESSION_MUTATION_V1,
            FEATURE_TURN_CONTROL_V1,
            FEATURE_VAULT_STAGE_V1,
        ])
    );
}

#[test]
fn provider_models_unavailable_reason_is_typed_and_golden() {
    let frame = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error {
                        data: Some(haider_rpc::ErrorData::ProviderModelsUnavailable { .. }),
                        ..
                    },
                    ..
                }
            )
        })
        .expect("typed model-unavailable golden");
    let value = serde_json::to_value(frame).expect("error JSON");
    assert_eq!(
        value["body"]["data"],
        serde_json::json!({
            "kind": "provider_models_unavailable",
            "provider": "anthropic-oauth",
            "reason": "provider did not serve a list to this credential"
        })
    );
}
