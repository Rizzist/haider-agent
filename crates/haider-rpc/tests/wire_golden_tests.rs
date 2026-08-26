//! Golden JSON shape and serde tolerance tests.
#![allow(clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use common::{TEST_FRAME_LIMIT, transcript};
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_rpc::{
    CancelStatus, DEFAULT_FRAME_LIMIT, ERROR_CODE_ALREADY_RESOLVED, ERROR_CODE_ARTIFACT_TOO_LARGE,
    ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED, ERROR_CODE_ATTACHMENT_NOT_FOUND,
    ERROR_CODE_ATTACHMENT_TOO_LARGE, ERROR_CODE_ATTACHMENTS_TOO_LARGE, ERROR_CODE_BUSY,
    ERROR_CODE_CAPABILITY_DENIED, ERROR_CODE_CREDENTIAL_MISSING, ERROR_CODE_CURSOR_AHEAD,
    ERROR_CODE_DRAINING, ERROR_CODE_INVALID_ARGUMENT, ERROR_CODE_INVALID_CURSOR,
    ERROR_CODE_NOT_FOUND, ERROR_CODE_OVERLOADED, ERROR_CODE_PERMISSION_DENIED,
    ERROR_CODE_PROVIDER_ERROR, ERROR_CODE_PROVIDER_REMOVE_REFUSED, ERROR_CODE_RESTAGE_REQUIRED,
    ERROR_CODE_REVISION_CONFLICT, ERROR_CODE_RUN_NOT_ACTIVE, ERROR_CODE_STALE_GENERATION,
    ERROR_CODE_TOO_MANY_ATTACHMENTS, ERROR_CODE_UNAUTHORIZED, ERROR_CODE_VAULT_UNSUPPORTED,
    ERROR_CODE_VISION_UNSUPPORTED, ErrorData, FEATURE_ACCOUNT_LOGIN_API_V1,
    FEATURE_ACCOUNT_MANAGEMENT_V1, FEATURE_ACCOUNT_OAUTH_DEVICE_V1, FEATURE_ACCOUNT_OAUTH_PKCE_V1,
    FEATURE_ACCOUNT_ROTATION_V1, FEATURE_PROVIDER_CONFIGURE_V1, FEATURE_PROVIDER_MANAGEMENT_V1,
    FEATURE_PROVIDER_MODELS_V1, FEATURE_PROVIDER_REMOVE_V1, FEATURE_SESSION_MUTATION_V1,
    FEATURE_TURN_CONTROL_V1, FEATURE_VAULT_STAGE_V1, Hello, RequestBody, ResponseBody,
    SubmitDisposition, WIRE_PROTOCOL_VERSION, Welcome, WireFrame, uds_codec, ws_codec,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("wire_transcript.json")
}

fn contract_methods_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("client_contract_methods_v1.json")
}

fn availability_compat_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("snapshot_availability_compat_v1.json")
}

#[test]
fn provider_auth_oauth_wire_wart_is_stable_and_documented() {
    let encoded = serde_json::to_string(&haider_rpc::ProviderAuthRequirementWire::OAuth)
        .expect("serialize provider OAuth requirement");
    assert_eq!(
        encoded, r#""o_auth""#,
        "the shipped ProviderAuthRequirementWire spelling must not be normalized"
    );

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let audit = std::fs::read_to_string(workspace.join("docs/client-contract-v1-enum-audit.md"))
        .expect("read enum audit");
    assert!(
        audit.contains(
            "`ProviderAuthRequirementWire::{ApiKey, OAuth}` | `\"api_key\"`, `\"o_auth\"`"
        ),
        "the initialism sweep must list the shipped o_auth wart"
    );
    let contract = std::fs::read_to_string(workspace.join("docs/client-contract-v1.md"))
        .expect("read client contract");
    // Lane 953b: defend the LF-bearing assertion against a Windows CRLF checkout.
    let contract = contract.replace("\r\n", "\n");
    assert!(
        contract.contains("Rust `OAuth` becomes\n`\"o_auth\"`"),
        "the contract must warn that source-derived snake case splits OAuth"
    );
}

fn request_methods_declared_in_source() -> BTreeSet<String> {
    let source =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/frame.rs"))
            .expect("read RequestBody source for fixture completeness");
    let (_, after_request) = source
        .split_once("pub enum RequestBody {")
        .expect("RequestBody declaration exists");
    let (request_source, _) = after_request
        .split_once("pub enum ResponseBody {")
        .expect("ResponseBody follows RequestBody");

    request_source
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("#[serde(rename = \"")
                .and_then(|rest| rest.split_once('"'))
                .map(|(method, _)| method.to_owned())
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ContractMethodFixture {
    contract: String,
    methods: Vec<ContractMethodPair>,
}

#[derive(Debug, Deserialize)]
struct ContractMethodPair {
    request_method: String,
    response_method: String,
    request: Value,
    response: Value,
}

#[derive(Debug, Deserialize)]
struct AvailabilityCompatFixture {
    contract: String,
    pairs: Vec<AvailabilityCompatPair>,
}

#[derive(Debug, Deserialize)]
struct AvailabilityCompatPair {
    method: String,
    old: Value,
    new: Value,
}

#[derive(Debug, Deserialize)]
struct LegacyAccountListResponse {
    method: String,
    #[serde(default)]
    descriptors: Vec<Value>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    provider_active: Vec<Value>,
    #[serde(default)]
    provider_defaults: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct LegacyProviderListResponse {
    method: String,
    #[serde(default)]
    providers: Vec<Value>,
    revision: u64,
}

#[derive(Debug, Deserialize)]
struct LegacyUsageReportResponse {
    method: String,
    report: Value,
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

fn wire_method(value: &Value) -> &str {
    value
        .get("method")
        .and_then(Value::as_str)
        .expect("fixture body has a method")
}

/// Every v1 request method has a golden request and its successful response.
/// The historical transcript supplies the original 40 methods; the focused
/// contract fixture supplies every subsequently added door. Adding a request
/// method without adding a fixture must make this pin red.
#[test]
fn every_request_method_has_a_golden_request_and_success_response() {
    const EXPECTED_METHODS: &[&str] = &[
        "account.add",
        "account.device_candidates",
        "account.import_device",
        "account.list",
        "account.list_watch",
        "account.login_api",
        "account.oauth_cancel",
        "account.oauth_import",
        "account.oauth_import_sources",
        "account.oauth_start",
        "account.oauth_status",
        "account.remove",
        "account.set_active",
        "account.set_default_model",
        "account.set_label",
        "agent.message",
        "artifact.put",
        "branch.create",
        "command.invoke",
        "command.list",
        "computer.permission_open_settings",
        "daemon.shutdown",
        "graph.abandon",
        "graph.inspect",
        "graph.pin",
        "graph.run_set.open",
        "graph.status",
        "graph.switch",
        "hooks.list",
        "hooks.revoke",
        "hooks.trust",
        "loom.list",
        "loom.register_agent_type",
        "loom.register_workflow",
        "provider.configure",
        "provider.list",
        "provider.models_refresh",
        "provider.remove",
        "queue.list",
        "queue.promote_steer",
        "queue.remove",
        "run.retry",
        "session.attach",
        "session.compact",
        "session.create",
        "session.detach",
        "session.diagnostic",
        "session.fleet",
        "session.fork",
        "session.input_inject",
        "session.list",
        "session.list_watch",
        "session.metafork",
        "session.observe",
        "session.observe_batch",
        "session.pipe_path",
        "session.read",
        "session.rename",
        "session.seen",
        "session.select_agent_type",
        "session.select_effort",
        "session.select_fast",
        "session.select_model",
        "session.surface_publish",
        "session.surface_watch",
        "shell.exec",
        "tools.inventory",
        "transcription.secret_get",
        "transcription.secret_set",
        "turn.cancel",
        "turn.submit",
        "turn.submit_from_cli",
        "turn.submit_with_hook_trust",
        "usage.report",
        "usage.history_day",
        "usage.history_range",
        "vault.stage",
    ];

    let expected_methods = EXPECTED_METHODS
        .iter()
        .map(|method| (*method).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        request_methods_declared_in_source(),
        expected_methods,
        "RequestBody changed without updating the exhaustive client-contract fixture matrix"
    );

    let mut requests_by_id = BTreeMap::new();
    let mut responses_by_id = BTreeMap::new();
    for frame in transcript() {
        match frame {
            WireFrame::Request { request_id, body } => {
                let value = serde_json::to_value(body).expect("encode transcript request");
                requests_by_id.insert(request_id.0, wire_method(&value).to_owned());
            }
            WireFrame::Response { request_id, body }
                if !matches!(body, ResponseBody::Error { .. }) =>
            {
                let value = serde_json::to_value(body).expect("encode transcript response");
                responses_by_id.insert(request_id.0, wire_method(&value).to_owned());
            }
            _ => {}
        }
    }

    let mut covered = BTreeSet::new();
    for (request_id, request_method) in requests_by_id {
        if responses_by_id.contains_key(&request_id) {
            covered.insert(request_method);
        }
    }

    let fixture: ContractMethodFixture = serde_json::from_str(
        &std::fs::read_to_string(contract_methods_fixture_path())
            .expect("read client contract method fixture"),
    )
    .expect("decode client contract method fixture");
    assert_eq!(fixture.contract, "haider-client-wire/v1");
    for pair in fixture.methods {
        assert_eq!(wire_method(&pair.request), pair.request_method);
        assert_eq!(wire_method(&pair.response), pair.response_method);

        let request: RequestBody =
            serde_json::from_value(pair.request.clone()).expect("decode golden request");
        let response: ResponseBody =
            serde_json::from_value(pair.response.clone()).expect("decode golden response");
        assert_eq!(
            serde_json::to_value(request).expect("re-encode golden request"),
            pair.request,
            "non-canonical request fixture for {}",
            pair.request_method
        );
        assert_eq!(
            serde_json::to_value(response).expect("re-encode golden response"),
            pair.response,
            "non-canonical response fixture for {}",
            pair.request_method
        );
        assert!(
            covered.insert(pair.request_method.clone()),
            "duplicate method fixture: {}",
            pair.request_method
        );
    }

    assert_eq!(covered, expected_methods);
}

/// v0.0.942-shaped readers ignore the additive availability field, while the
/// current reader preserves omission as `None`. This is the two-direction N-1
/// compatibility promise in executable form.
#[test]
fn snapshot_availability_is_compatible_in_both_n_minus_one_directions() {
    let fixture: AvailabilityCompatFixture = serde_json::from_str(
        &std::fs::read_to_string(availability_compat_fixture_path())
            .expect("read availability compatibility fixture"),
    )
    .expect("decode availability compatibility fixture");
    assert_eq!(fixture.contract, "haider-client-wire/v1");

    for pair in fixture.pairs {
        let current_from_old: ResponseBody =
            serde_json::from_value(pair.old.clone()).expect("new client decodes old response");
        let current_from_new: ResponseBody =
            serde_json::from_value(pair.new.clone()).expect("new client decodes new response");

        match (pair.method.as_str(), &current_from_old, &current_from_new) {
            (
                "account.list",
                ResponseBody::AccountList {
                    availability: None, ..
                },
                ResponseBody::AccountList {
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
                    ..
                },
            ) => {
                let legacy: LegacyAccountListResponse = serde_json::from_value(pair.new.clone())
                    .expect("old client decodes new account response");
                assert_eq!(legacy.method, "account.list");
                assert!(legacy.descriptors.is_empty());
                assert!(legacy.revision.is_none());
                assert!(legacy.provider_active.is_empty());
                assert!(legacy.provider_defaults.is_empty());
            }
            (
                "provider.list",
                ResponseBody::ProviderList {
                    availability: None, ..
                },
                ResponseBody::ProviderList {
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Unavailable { reason }),
                    ..
                },
            ) => {
                assert_eq!(reason, "provider subsystem is not configured");
                let legacy: LegacyProviderListResponse = serde_json::from_value(pair.new.clone())
                    .expect("old client decodes new provider response");
                assert_eq!(legacy.method, "provider.list");
                assert!(legacy.providers.is_empty());
                assert_eq!(legacy.revision, 0);
            }
            (
                "usage.report",
                ResponseBody::UsageReport {
                    availability: None, ..
                },
                ResponseBody::UsageReport {
                    availability: Some(haider_rpc::SnapshotAvailabilityWire::Unavailable { reason }),
                    ..
                },
            ) => {
                assert_eq!(reason, "usage subsystem is not configured");
                let legacy: LegacyUsageReportResponse = serde_json::from_value(pair.new.clone())
                    .expect("old client decodes new usage response");
                assert_eq!(legacy.method, "usage.report");
                assert!(legacy.report.is_object());
            }
            _ => panic!("unexpected compatibility fixture for {}", pair.method),
        }

        assert_eq!(
            serde_json::to_value(current_from_old).expect("re-encode old response"),
            pair.old,
            "omitted availability must remain omitted for {}",
            pair.method
        );
        assert_eq!(
            serde_json::to_value(current_from_new).expect("re-encode new response"),
            pair.new,
            "present availability must remain explicit for {}",
            pair.method
        );
    }

    let future_state: haider_rpc::SnapshotAvailabilityWire = serde_json::from_value(
        serde_json::json!({"state": "temporarily_degraded", "retry_after_ms": 5000}),
    )
    .expect("unknown future availability state remains decodable");
    assert_eq!(future_state, haider_rpc::SnapshotAvailabilityWire::Unknown);
}

/// OAuth import-source reason codes are extensible while their prose remains
/// displayable.
///
/// MUTATION CHECK: remove `#[serde(other)]` from
/// `OAuthImportSourceUnavailableCodeWire::Unknown`. Expected runtime failure:
/// decoding the synthetic future code below fails instead of preserving the
/// human message.
#[test]
fn unknown_oauth_import_source_reason_code_preserves_prose() {
    let future = r#"{
        "method":"account.oauth_import_sources",
        "sources":[{
            "source":"codex",
            "provider":"openai-oauth",
            "default_alias":"openai-oauth",
            "available":false,
            "unavailable_reason":{
                "code":"credential_store_moved",
                "message":"The credential store moved; update the daemon and refresh."
            }
        }]
    }"#;
    let body: ResponseBody = serde_json::from_str(future).expect("future reason code decodes");
    let ResponseBody::AccountOAuthImportSources { sources } = body else {
        panic!("expected OAuth import-source catalog response");
    };
    let reason = sources[0]
        .unavailable_reason
        .as_ref()
        .expect("future unavailable reason remains present");
    assert_eq!(
        reason.code,
        haider_rpc::OAuthImportSourceUnavailableCodeWire::Unknown
    );
    assert_eq!(
        reason.message,
        "The credential store moved; update the daemon and refresh."
    );
}

/// MUTATION CHECK: make `accepted_proposal_digest` required or serialize it as
/// null. Expected RUNTIME failure: the write-free review request below no
/// longer has the additive old-reader-friendly shape.
///
/// MUTATION CHECK: remove the typed proposal/range fields. Expected RUNTIME
/// failure: the operator review cannot show the exact model-proposed history
/// removal before a metafork commit.
#[test]
fn session_metafork_review_shape_is_additive_and_exact() {
    let json = serde_json::json!({
        "method": "session.metafork",
        "command_id": "meta-command",
        "session_id": "source-session",
        "worker_generation": 7,
        "fork_node_id": "fork-node",
        "fork_seq": 19,
        "description": "remove parts about chocolate",
        "model_proposal": {
            "removals": [{
                "from_seq": 8,
                "through_seq": 11,
                "reason": "chocolate discussion",
                "preview": "tempering chocolate"
            }]
        },
        "future_additive_field": true
    });
    let decoded: RequestBody = serde_json::from_value(json).expect("metafork review request");
    let RequestBody::SessionMetafork {
        accepted_proposal_digest,
        model_proposal,
        ..
    } = &decoded
    else {
        panic!("typed metafork request");
    };
    assert!(accepted_proposal_digest.is_none());
    assert_eq!(model_proposal.removals[0].from_seq, 8);
    assert_eq!(
        model_proposal.removals[0].preview.as_deref(),
        Some("tempering chocolate")
    );
    let encoded = serde_json::to_value(decoded).expect("metafork re-encode");
    assert!(encoded.get("accepted_proposal_digest").is_none());
    assert!(encoded.get("future_additive_field").is_none());
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
    let golden = std::fs::read_to_string(path)
        .expect("missing wire fixture; run with UPDATE_FIXTURES=1")
        // Checked-out Windows text can be CRLF, but protocol bytes are frozen
        // as canonical LF JSON independently of the host checkout policy.
        .replace("\r\n", "\n");
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

/// ST1 submit-wire law. MUTATION CHECK: omit the mode, map Subturn to Steer,
/// or reject the additive string while normalizing the legacy submit shape.
/// Expected runtime failure: the encoded mode or decoded variant differs.
#[test]
fn turn_submit_round_trips_subturn_mode() {
    let submit = RequestBody::TurnSubmit {
        command_id: haider_rpc::CommandId::new("subturn-submit"),
        session_id: haider_protocol::ids::SessionId::new("subturn-session"),
        worker_generation: 7,
        text: "revise the pending call".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Subturn,
    };
    let value = serde_json::to_value(&submit).expect("encode subturn submit");
    assert_eq!(value["method"], "turn.submit");
    assert_eq!(value["mode"], "subturn");
    let decoded: RequestBody = serde_json::from_value(value).expect("decode subturn submit");
    assert!(matches!(
        decoded,
        RequestBody::TurnSubmitWithBranch {
            mode: haider_protocol::DeliveryMode::Subturn,
            branch_id: None,
            ..
        }
    ));
}

/// The native sidecar lookup is a typed read RPC; pin both discriminants so a
/// client never has to infer a filesystem path from the session id.
#[test]
fn session_pipe_path_request_and_response_have_stable_wire_shapes() {
    let session_id = haider_protocol::ids::SessionId::new("pipe-path-wire-session");
    assert_eq!(
        serde_json::to_value(RequestBody::SessionPipePath {
            session_id: session_id.clone(),
        })
        .expect("encode pipe path request"),
        serde_json::json!({
            "method": "session.pipe_path",
            "session_id": "pipe-path-wire-session",
        })
    );
    assert_eq!(
        serde_json::to_value(ResponseBody::SessionPipePath {
            path: "/profile/pipe/pipe-path-wire-session.pipe".into(),
        })
        .expect("encode pipe path response"),
        serde_json::json!({
            "method": "session.pipe_path",
            "path": "/profile/pipe/pipe-path-wire-session.pipe",
        })
    );
    let decoded: RequestBody = serde_json::from_value(serde_json::json!({
        "method": "session.pipe_path",
        "session_id": session_id,
    }))
    .expect("decode pipe path request");
    assert!(matches!(decoded, RequestBody::SessionPipePath { .. }));
}

/// MUTATION CHECK: charge a 5 MiB image as if the default frame carried raw
/// bytes, or shrink the documented bound below the padded base64 envelope.
/// Expected RUNTIME failure: encoding returns `FrameTooLarge` instead of a
/// request that fits the already-negotiated default limit.
#[test]
fn five_mib_artifact_put_fits_the_default_negotiated_frame() {
    let raw_bytes: usize = 5 * 1024 * 1024;
    let padded_base64_bytes = raw_bytes.div_ceil(3) * 4;
    let frame = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("five-mib-put"),
        body: RequestBody::ArtifactPut {
            data_base64: "A".repeat(padded_base64_bytes),
        },
    };
    let encoded = ws_codec::encode(&frame, DEFAULT_FRAME_LIMIT).expect("5 MiB put fits");
    assert!(encoded.len() < DEFAULT_FRAME_LIMIT);
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
        user_command_withheld,
        ..
    }) = welcome
    else {
        panic!("expected welcome");
    };
    assert!(profile_id.is_empty());
    assert!(daemon_version.is_empty());
    assert!(features.is_empty());
    assert!(!user_command_withheld);
}

/// MUTATION CHECK: make `permission_overrides` required or default either
/// boolean to true. Expected RUNTIME failure: the legacy create fails to
/// decode or no longer normalizes to a fail-closed `None` override.
#[test]
fn legacy_session_create_defaults_permission_overrides_to_none() {
    let json = r#"{"v":1,"kind":"request","request_id":"legacy-create","body":{"method":"session.create","command_id":"legacy-command","cwd":"/tmp","provider":"fake","model":"fake-model","max_tokens":4096}}"#;
    let decoded = serde_json::from_str::<WireFrame>(json).expect("legacy create decodes");
    assert_eq!(
        decoded,
        WireFrame::Request {
            request_id: haider_rpc::RequestId::new("legacy-create"),
            body: RequestBody::SessionCreateWithPermissionOverrides {
                command_id: haider_rpc::CommandId::new("legacy-command"),
                cwd: "/tmp".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                max_tokens: 4096,
                permission_overrides: None,
                cache_policy: None,
            },
        }
    );

    let overrides = SessionPermissionOverridesV1 {
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    };
    assert!(!overrides.is_empty());
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

/// MUTATION CHECK: rename an attachment refusal or collapse its structured
/// recovery coordinates into a generic error. Expected RUNTIME failure: the
/// literal or tagged `ErrorData` kind no longer matches the public wire law.
#[test]
fn attachment_error_codes_and_data_are_typed_additively() {
    assert_eq!(
        [
            ERROR_CODE_ARTIFACT_TOO_LARGE,
            ERROR_CODE_ATTACHMENT_NOT_FOUND,
            ERROR_CODE_ATTACHMENT_MIME_UNSUPPORTED,
            ERROR_CODE_ATTACHMENT_TOO_LARGE,
            ERROR_CODE_TOO_MANY_ATTACHMENTS,
            ERROR_CODE_ATTACHMENTS_TOO_LARGE,
            ERROR_CODE_VISION_UNSUPPORTED,
        ],
        [
            "artifact_too_large",
            "attachment_not_found",
            "attachment_mime_unsupported",
            "attachment_too_large",
            "too_many_attachments",
            "attachments_too_large",
            "vision_unsupported",
        ]
    );
    let tagged = [
        (
            ErrorData::AttachmentNotFound {
                index: 1,
                artifact: haider_rpc::haider_protocol::ids::ArtifactRef::new("blake3:missing"),
            },
            "attachment_not_found",
        ),
        (
            ErrorData::AttachmentMimeUnsupported {
                index: 2,
                mime: "image/svg+xml".into(),
            },
            "attachment_mime_unsupported",
        ),
        (
            ErrorData::TooManyAttachments {
                actual_count: 6,
                max_count: 5,
            },
            "too_many_attachments",
        ),
        (
            ErrorData::VisionUnsupported {
                provider: "fake".into(),
            },
            "vision_unsupported",
        ),
    ];
    for (data, expected_kind) in tagged {
        assert_eq!(
            serde_json::to_value(data).expect("typed attachment error")["kind"],
            expected_kind
        );
    }
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
    assert!(matches!(
        body,
        RequestBody::SessionCreateWithPermissionOverrides {
            permission_overrides: None,
            ..
        }
    ));
}

/// Older-client tolerance for the session.list roster-truth fields, both
/// directions: an OLDER daemon's summary (no turn/footprint fields) must
/// decode with every roster field `None` — absence is "unknown", never
/// zero — and a NEWER daemon's enriched summary must decode by a client
/// that does not know the NEXT additive field either.
///
/// MUTATION CHECK: make `SessionSummary` strict about unknown fields, or
/// default a missing `turn_count`/`footprint_tokens` to `Some(0)`. Expected
/// failure: one of the decodes below rejects, or the older-daemon row stops
/// reading as unknown.
#[test]
fn session_summary_roster_truth_fields_are_additive_and_tolerated() {
    let older_daemon = r#"{
        "method":"session.list",
        "sessions":[{"session_id":"session-1","head_seq":9,"worker_generation":7}]
    }"#;
    let body: ResponseBody = serde_json::from_str(older_daemon).expect("older summary decodes");
    let ResponseBody::SessionList { sessions, .. } = body else {
        panic!("expected session.list body");
    };
    assert_eq!(sessions[0].turn_count, None);
    assert_eq!(sessions[0].footprint_tokens, None);
    assert_eq!(sessions[0].footprint_truth, None);
    assert_eq!(sessions[0].agent_metrics, None);
    assert_eq!(sessions[0].workspace_cwd, None);

    let newer_daemon = r#"{
        "method":"session.list",
        "sessions":[{
            "session_id":"session-1",
            "head_seq":9,
            "worker_generation":7,
            "turn_count":4,
            "footprint_tokens":33500,
            "footprint_truth":"exact",
            "agent_metrics":{
                "session_id":"session-1",
                "head_seq":9,
                "started_at_ms":100,
                "live":true,
                "tool_attempts":2,
                "usage":{
                    "logical_input_tokens":1000,
                    "billed_output_tokens":50,
                    "additional_reasoning_tokens":0,
                    "cache_read_tokens":800,
                    "cache_write_tokens":0,
                    "metered_cost_microusd":123000,
                    "api_equivalent_cost_microusd":123000,
                    "all_lanes_priced":true,
                    "has_metered_lanes":true,
                    "has_oauth_lanes":false,
                    "breakdowns":[]
                }
            },
            "future_roster_field":true
        }]
    }"#;
    let body: ResponseBody = serde_json::from_str(newer_daemon).expect("newer summary decodes");
    let ResponseBody::SessionList { sessions, .. } = body else {
        panic!("expected session.list body");
    };
    assert_eq!(sessions[0].turn_count, Some(4));
    assert_eq!(sessions[0].footprint_tokens, Some(33_500));
    assert_eq!(
        sessions[0].footprint_truth,
        Some(haider_protocol::context::ContextFootprintTruth::Exact)
    );
    let metrics = sessions[0].agent_metrics.as_ref().expect("agent metrics");
    assert_eq!(metrics.head_seq, 9);
    assert_eq!(metrics.tool_attempts, 2);
    assert!(metrics.live);
    assert_eq!(
        metrics
            .usage
            .as_ref()
            .and_then(|usage| usage.metered_cost_microusd),
        Some(123_000)
    );
}

/// WIRE-GAPS item 4 is a plain optional summary coordinate: new readers
/// preserve absence from older daemons, and an older field projection ignores
/// the current writer's workspace path.
#[test]
fn session_summary_workspace_is_additive_and_old_decoder_tolerant() {
    let current = haider_rpc::SessionSummary {
        session_id: haider_protocol::ids::SessionId::new("session-workspace"),
        head_seq: 17,
        worker_generation: 15,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        workspace_cwd: Some("/work/original".into()),
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
    };
    let value = serde_json::to_value(&current).expect("encode current summary");
    assert_eq!(value["workspace_cwd"], "/work/original");

    #[derive(Deserialize)]
    struct LegacySummary {
        session_id: haider_protocol::ids::SessionId,
        head_seq: u64,
        worker_generation: u64,
    }
    let legacy: LegacySummary =
        serde_json::from_value(value).expect("legacy summary ignores workspace_cwd");
    assert_eq!(legacy.session_id.as_str(), "session-workspace");
    assert_eq!(legacy.head_seq, 17);
    assert_eq!(legacy.worker_generation, 15);

    let older: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "session-workspace",
        "head_seq": 17,
        "worker_generation": 15,
    }))
    .expect("older summary decodes");
    assert_eq!(older.workspace_cwd, None);
}

/// A 0.0.942 summary has no promoted provider field. New readers retain the
/// nested provider for compatibility while treating the top-level location as
/// unknown; decoding must never manufacture the promotion.
///
/// MUTATION CHECK (executed): remove the explicit `default` from
/// `SessionSummary.provider`. Expected RUNTIME failure: the wire-law assertion
/// rejects the declaration. Serde currently gives missing `Option<T>` fields
/// an implicit `None`, so the real payload decode below intentionally remains
/// alongside the source-level guard rather than pretending it pins the
/// required explicit annotation by itself.
#[test]
fn session_summary_without_top_level_provider_still_decodes() {
    // Lane 953b: defend the LF-bearing assertion against a Windows CRLF checkout.
    let frame = include_str!("../src/frame.rs").replace("\r\n", "\n");
    assert!(
        frame.contains(
            "#[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub provider: Option<String>"
        ),
        "provider must retain the explicit additive wire law"
    );
    let v0_0_942 = serde_json::json!({
        "session_id": "session-provider-compat",
        "head_seq": 17,
        "worker_generation": 15,
        "metadata": {
            "cwd": "/work/original",
            "provider": "anthropic",
            "model": "claude-sonnet",
            "max_tokens": 4096,
            "created_at_ms": 1_800_000_000_000_u64
        },
        "last_model": "claude-sonnet"
    });
    let decoded: haider_rpc::SessionSummary =
        serde_json::from_value(v0_0_942).expect("0.0.942 summary decodes");
    assert_eq!(decoded.provider, None, "absence remains unknown");
    assert_eq!(
        decoded
            .metadata
            .as_ref()
            .map(|metadata| metadata.provider.as_str()),
        Some("anthropic"),
        "the compatibility copy remains readable"
    );
}

/// A v0.0.942 summary predates both promoted cache-rate scalars. New readers
/// must decode that exact shape without inventing either value, while the
/// explicit serde declaration keeps the additive wire contract visible.
///
/// MUTATION `REMOVE_CACHE_FIELD_DEFAULT` (executed, observed red): remove the
/// explicit `default` from either new field. Serde implicitly defaults a
/// missing `Option`, so the enclosing source-law assertion (not the payload
/// decode alone) is what turns this mutation red.
#[test]
fn v0_0_942_session_summary_without_promoted_cache_rates_still_decodes() {
    // Lane 953b: defend the LF-bearing assertion against a Windows CRLF checkout.
    let frame = include_str!("../src/frame.rs").replace("\r\n", "\n");
    for declaration in [
        "#[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub cache_lifetime_hit_basis_points: Option<u32>",
        "#[serde(default, skip_serializing_if = \"Option::is_none\")]\n    pub cache_reread_hit_basis_points: Option<u32>",
    ] {
        assert!(
            frame.contains(declaration),
            "cache-rate fields must retain the explicit additive wire law: {declaration}"
        );
    }
    let v0_0_942 = serde_json::json!({
        "session_id": "session-cache-compat",
        "head_seq": 17,
        "worker_generation": 15,
        "last_model": "gpt-5.2"
    });
    let decoded: haider_rpc::SessionSummary =
        serde_json::from_value(v0_0_942).expect("0.0.942 summary decodes");
    assert_eq!(decoded.cache_lifetime_hit_basis_points, None);
    assert_eq!(decoded.cache_reread_hit_basis_points, None);

    #[derive(Deserialize)]
    struct V0_0_942Summary {
        session_id: haider_protocol::ids::SessionId,
        head_seq: u64,
        worker_generation: u64,
    }
    let v0_0_943 = serde_json::json!({
        "session_id": "session-cache-compat",
        "head_seq": 18,
        "worker_generation": 16,
        "cache_lifetime_hit_basis_points": 6370,
        "cache_reread_hit_basis_points": 9058
    });
    let legacy: V0_0_942Summary =
        serde_json::from_value(v0_0_943).expect("0.0.942 reader ignores promoted fields");
    assert_eq!(legacy.session_id.as_str(), "session-cache-compat");
    assert_eq!(legacy.head_seq, 18);
    assert_eq!(legacy.worker_generation, 16);
}

/// Absence at each retained nesting level remains inspectable even though all
/// three cases correctly provide no re-read number. This is why promotion is
/// additive: old-session diagnostics still distinguish no snapshot, a
/// snapshot without usage, and measured usage without a re-readable prefix.
///
/// MUTATION `COLLAPSE_CACHE_ABSENCES` (executed, observed red): omit
/// `agent_metrics` whenever its usage/rate is absent. Expected RUNTIME
/// failure: the structural assertions can no longer distinguish the rows.
#[test]
fn session_summary_cache_absence_levels_remain_distinguishable() {
    let decode = |value| {
        serde_json::from_value::<haider_rpc::SessionSummary>(value).expect("summary shape decodes")
    };
    let no_metrics = decode(serde_json::json!({
        "session_id": "cache-absence-no-metrics",
        "head_seq": 1,
        "worker_generation": 1
    }));
    let no_usage = decode(serde_json::json!({
        "session_id": "cache-absence-no-usage",
        "head_seq": 2,
        "worker_generation": 1,
        "agent_metrics": {
            "session_id": "cache-absence-no-usage",
            "head_seq": 2,
            "started_at_ms": 1,
            "live": true,
            "tool_attempts": 0
        }
    }));
    let no_rate = decode(serde_json::json!({
        "session_id": "cache-absence-no-rate",
        "head_seq": 3,
        "worker_generation": 1,
        "cache_lifetime_hit_basis_points": 0,
        "agent_metrics": {
            "session_id": "cache-absence-no-rate",
            "head_seq": 3,
            "started_at_ms": 1,
            "live": true,
            "tool_attempts": 0,
            "usage": {
                "logical_input_tokens": 4098,
                "billed_output_tokens": 0,
                "additional_reasoning_tokens": 0,
                "cache_read_tokens": 0,
                "cache_write_tokens": 0,
                "cache_hit_basis_points": 0,
                "all_lanes_priced": false,
                "has_metered_lanes": false,
                "has_oauth_lanes": true,
                "breakdowns": []
            }
        }
    }));

    assert!(no_metrics.agent_metrics.is_none());
    assert!(
        no_usage
            .agent_metrics
            .as_ref()
            .is_some_and(|metrics| metrics.usage.is_none())
    );
    assert!(
        no_rate
            .agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
            .is_some_and(|usage| usage.cache_reread_hit_basis_points.is_none())
    );

    let no_metrics_wire = serde_json::to_value(no_metrics).expect("no-metrics serializes");
    let no_usage_wire = serde_json::to_value(no_usage).expect("no-usage serializes");
    let no_rate_wire = serde_json::to_value(no_rate).expect("no-rate serializes");
    assert!(no_metrics_wire.get("agent_metrics").is_none());
    assert!(no_usage_wire["agent_metrics"].get("usage").is_none());
    assert!(no_rate_wire["agent_metrics"]["usage"].is_object());
    assert!(no_rate_wire.get("cache_reread_hit_basis_points").is_none());
    assert_eq!(no_rate_wire["cache_lifetime_hit_basis_points"], 0);
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
    assert!(matches!(
        body,
        RequestBody::TurnSubmitWithBranch {
            branch_id: None,
            ..
        }
    ));
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

#[test]
fn shell_exec_run_id_is_additive_in_both_decode_directions() {
    let old_request: RequestBody = serde_json::from_str(
        r#"{
            "method":"shell.exec",
            "command_id":"shell-command-1",
            "session_id":"session-1",
            "worker_generation":7,
            "command":"printf ok"
        }"#,
    )
    .expect("pre-scope shell request decodes");
    assert!(matches!(
        old_request,
        RequestBody::ShellExecScoped {
            branch_id: None,
            agent_id: None,
            ..
        }
    ));

    let old: ResponseBody = serde_json::from_str(
        r#"{
            "method":"shell.exec",
            "session_id":"session-1",
            "item_id":"shell-item-1",
            "accepted_seq":51,
            "worker_generation":7
        }"#,
    )
    .expect("pre-run-id shell response decodes");
    assert!(matches!(old, ResponseBody::ShellExec { run_id: None, .. }));

    let current = ResponseBody::ShellExec {
        session_id: haider_protocol::ids::SessionId::new("session-1"),
        run_id: Some(haider_protocol::ids::RunId::new("shell-run-1")),
        item_id: haider_protocol::ids::ItemId::new("shell-item-1"),
        accepted_seq: 51,
        worker_generation: 7,
    };
    #[derive(Deserialize)]
    #[serde(tag = "method")]
    enum LegacyResponse {
        #[serde(rename = "shell.exec")]
        ShellExec {
            session_id: haider_protocol::ids::SessionId,
            item_id: haider_protocol::ids::ItemId,
            accepted_seq: u64,
            worker_generation: u64,
        },
    }
    let legacy: LegacyResponse =
        serde_json::from_value(serde_json::to_value(current).expect("current shell response JSON"))
            .expect("legacy client ignores additive run id");
    let LegacyResponse::ShellExec {
        session_id,
        item_id,
        accepted_seq,
        worker_generation,
    } = legacy;
    assert_eq!(session_id.as_str(), "session-1");
    assert_eq!(item_id.as_str(), "shell-item-1");
    assert_eq!(accepted_seq, 51);
    assert_eq!(worker_generation, 7);
}

/// MUTATION CHECK: serialize run-scoped trust as an ordinary turn (changing
/// old receipt bytes) or drop the additive hooks RPC methods. Expected RUNTIME
/// failure: one of these exact discriminants/coordinates changes.
#[test]
fn hooks_rpc_and_run_scoped_trust_shapes_round_trip_exactly() {
    let submit = RequestBody::TurnSubmitWithHookTrust {
        command_id: haider_rpc::CommandId::new("submit-hooks-1"),
        session_id: haider_protocol::ids::SessionId::new("session-hooks-1"),
        worker_generation: 7,
        branch_id: None,
        text: "run with hooks".into(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Queue,
    };
    let value = serde_json::to_value(&submit).expect("encode trusted submit");
    assert_eq!(value["method"], "turn.submit_with_hook_trust");
    assert!(value.get("branch_id").is_none());
    assert_eq!(
        serde_json::from_value::<RequestBody>(value).expect("decode trusted submit"),
        submit
    );

    let list = RequestBody::HooksList {
        cwd: "/workspace".into(),
    };
    let value = serde_json::to_value(&list).expect("encode hooks list");
    assert_eq!(
        value,
        serde_json::json!({
            "method": "hooks.list",
            "cwd": "/workspace"
        })
    );
    assert_eq!(
        serde_json::from_value::<RequestBody>(value).expect("decode hooks list"),
        list
    );
}

/// WIRE-GAPS items 2-3: current hook listings carry one monotonic revision
/// and daemon-owned trust classification. Both additions default cleanly for
/// an older daemon and are ignored by an older response projection.
#[test]
fn hooks_list_revision_and_trust_state_are_additive_and_tolerant() {
    let current = ResponseBody::HooksList {
        policy: "per_digest".into(),
        revision: 7,
        hooks: vec![haider_rpc::HookSummaryWire {
            name: "format".into(),
            digest: "d".repeat(64),
            source: "/work/original/hooks.json".into(),
            kind: "exec".into(),
            event: "run_finished".into(),
            trusted: false,
            trust_state: Some(haider_rpc::HookTrustStateWire::RevokedByEdit),
            decision: false,
            timeout_ms: 30_000,
        }],
    };
    let value = serde_json::to_value(&current).expect("encode current hook list");
    assert_eq!(value["revision"], 7);
    assert_eq!(value["hooks"][0]["trust_state"], "revoked_by_edit");

    #[derive(Deserialize)]
    struct LegacyHookSummary {
        name: String,
        trusted: bool,
    }
    #[derive(Deserialize)]
    #[serde(tag = "method")]
    enum LegacyResponseBody {
        #[serde(rename = "hooks.list")]
        HooksList {
            policy: String,
            hooks: Vec<LegacyHookSummary>,
        },
    }
    let legacy: LegacyResponseBody =
        serde_json::from_value(value).expect("legacy hook decoder ignores additions");
    let LegacyResponseBody::HooksList { policy, hooks } = legacy;
    assert_eq!(policy, "per_digest");
    assert_eq!(hooks[0].name, "format");
    assert!(!hooks[0].trusted);

    let older: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "hooks.list",
        "policy": "per_digest",
        "hooks": [{
            "name": "format",
            "digest": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "source": "/work/original/hooks.json",
            "kind": "exec",
            "event": "run_finished",
            "trusted": false,
            "decision": false,
            "timeout_ms": 30000
        }]
    }))
    .expect("older hook list decodes");
    let ResponseBody::HooksList {
        revision, hooks, ..
    } = older
    else {
        panic!("expected hooks.list")
    };
    assert_eq!(revision, 0);
    assert_eq!(hooks[0].trust_state, None);
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

/// MUTATION CHECK: make response methods closed instead of mapping an unknown
/// method to `Unknown`. Expected RUNTIME failure: the simulated pre-H1 client
/// can no longer decode a newer `session.observe`-family response additively.
#[test]
fn observe_methods_preserve_older_client_unknown_method_tolerance() {
    #[allow(dead_code)]
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum PreH1Frame {
        Response {
            request_id: String,
            body: PreH1ResponseBody,
        },
        #[serde(other)]
        Unknown,
    }

    #[allow(dead_code)]
    #[derive(Deserialize)]
    #[serde(tag = "method")]
    enum PreH1ResponseBody {
        #[serde(rename = "session.list")]
        SessionList,
        #[serde(other)]
        Unknown,
    }

    let frame = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response { request_id, .. }
                    if request_id.as_str() == "request-observe"
            )
        })
        .expect("H1 observe response remains in the additive transcript");
    let encoded = serde_json::to_string(&frame).expect("current observe response serializes");
    assert!(encoded.contains(r#""method":"session.observe""#));
    let decoded: PreH1Frame =
        serde_json::from_str(&encoded).expect("pre-H1 reader tolerates actual observe response");
    assert!(matches!(
        decoded,
        PreH1Frame::Response {
            request_id,
            body: PreH1ResponseBody::Unknown,
        } if request_id == "request-observe"
    ));
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
            user_code: None,
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

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum PreDeviceOAuthStatus {
        WaitingBrowser,
        Exchanging,
        #[serde(other)]
        Unknown,
    }
    let old_client_status: PreDeviceOAuthStatus =
        serde_json::from_str(r#"{"status":"waiting_device"}"#)
            .expect("pre-B6k client accepts the additive status");
    assert_eq!(old_client_status, PreDeviceOAuthStatus::Unknown);

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

    let device_welcome = transcript()
        .into_iter()
        .find_map(|frame| match frame {
            WireFrame::Welcome(welcome)
                if welcome.features
                    == std::collections::BTreeSet::from([
                        FEATURE_ACCOUNT_OAUTH_DEVICE_V1.to_owned()
                    ]) =>
            {
                Some(welcome)
            }
            _ => None,
        })
        .expect("device-flow feature Welcome");
    assert_eq!(
        serde_json::to_value(device_welcome).expect("device Welcome JSON")["features"],
        serde_json::json!([FEATURE_ACCOUNT_OAUTH_DEVICE_V1])
    );
    assert!(transcript().into_iter().any(|frame| matches!(
        frame,
        WireFrame::Response {
            body: ResponseBody::AccountOAuthStatus {
                status: haider_rpc::OAuthFlowStatusWire::WaitingDevice,
                ..
            },
            ..
        }
    )));
}

#[test]
fn oauth_status_shapes_cannot_carry_callback_or_token_secrets() {
    for status in [
        haider_rpc::OAuthFlowStatusWire::WaitingBrowser,
        haider_rpc::OAuthFlowStatusWire::WaitingDevice,
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
            availability: None,
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

/// B6a adds Gemini's native family without changing the protocol version.
///
/// MUTATION CHECK: remove `#[serde(other)]` from the pre-B6a decoder below.
/// Expected RUNTIME failure: the old-client decode rejects the new family
/// instead of preserving the provider row with an unknown family.
#[test]
fn api_family_wire_addition_tolerated_by_older_clients() {
    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum PreB6aProviderApiFamilyWire {
        AnthropicMessages,
        OpenaiResponses,
        OpenaiChatCompletions,
        #[serde(other)]
        Unknown,
    }

    let encoded = serde_json::to_string(&haider_rpc::ProviderApiFamilyWire::GeminiGenerateContent)
        .expect("Gemini family encode");
    assert_eq!(encoded, r#""gemini_generate_content""#);
    assert_eq!(
        serde_json::from_str::<PreB6aProviderApiFamilyWire>(&encoded)
            .expect("pre-B6a tolerant decode"),
        PreB6aProviderApiFamilyWire::Unknown
    );
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
        "provider.remove",
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
            WireFrame::Welcome(welcome) if welcome.features.len() == 11 => Some(welcome),
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
            FEATURE_PROVIDER_REMOVE_V1,
            FEATURE_SESSION_MUTATION_V1,
            FEATURE_TURN_CONTROL_V1,
            FEATURE_VAULT_STAGE_V1,
        ])
    );
}

/// Provider-removal refusals carry a stable code plus structured reason and
/// blocking aliases; clients never parse the human message.
///
/// MUTATION CHECK: drop `blocking_aliases` from the refusal data. Expected
/// RUNTIME failure: the exact typed JSON below loses both blocking names.
#[test]
fn provider_remove_refusal_reason_and_aliases_are_golden() {
    assert_eq!(
        ERROR_CODE_PROVIDER_REMOVE_REFUSED,
        "provider_remove_refused"
    );
    let frame = transcript()
        .into_iter()
        .find(|frame| {
            matches!(
                frame,
                WireFrame::Response {
                    body: ResponseBody::Error { code, .. },
                    ..
                } if code == ERROR_CODE_PROVIDER_REMOVE_REFUSED
            )
        })
        .expect("provider-remove refusal golden");
    let value = serde_json::to_value(frame).expect("provider-remove refusal JSON");
    assert_eq!(
        value["body"]["data"],
        serde_json::json!({
            "kind": "provider_remove_refused",
            "provider": "local-lab",
            "reason": "blocking_accounts",
            "blocking_aliases": ["lab-a", "lab-b"]
        })
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

/// LAW (D1): goldens additive + tolerance re-proved for the device
/// credential discovery surface.
///
/// Additivity is pinned two ways: the D1 frames are APPENDED to the golden
/// transcript (the fixture regeneration for this wave inserted entries only —
/// every pre-D1 golden byte is unchanged), and this test freezes the appended
/// surface's decode behavior for skewed peers. Tolerance is re-proved in both
/// directions: a current client decodes future-shaped D1 frames (extra
/// fields) into the typed variants, absent optional fields default, and the
/// CUT `account.refresh` method — which never became wire surface — decodes
/// as Unknown exactly like any other future method.
///
/// MUTATION CHECK: make `account_label` non-optional, stop skipping absent
/// optionals during encode, add a token-bearing field to the candidate wire,
/// or resurrect an `account.refresh` request variant. Expected runtime
/// failure: the matching assertion below (defaulted decode, key-absence
/// scan, no-token-field scan, or Unknown-method decode) breaks.
#[test]
fn device_discovery_goldens_are_additive_and_tolerance_re_proved() {
    use haider_rpc::{DeviceCredentialCandidateWire, FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1};

    // The feature bit is a pinned wire literal.
    assert_eq!(
        FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1,
        "account_device_discovery_v1"
    );

    // The D1 golden frames were appended at the then-END of the transcript:
    // every index before the D1 welcome predates that wave, so old fixture
    // entries could not have moved. Later waves (U1) append strictly AFTER
    // the D1 block; the U1 welcome is the fence that re-proves the D1 block
    // is still exactly its original six frames, untouched.
    let frames = transcript();
    let d1_start = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                WireFrame::Welcome(welcome)
                    if welcome.features.contains(FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1)
            )
        })
        .expect("D1 welcome frame in the golden transcript");
    // D1's six frames stay contiguous at their original offset; T1 then
    // appended seven transcription-secret frames, U1 three usage-report
    // frames, G2 three session-rename frames, G3 four tuning frames, and F1
    // three fleet frames AFTER them — each append pinned by its own additive
    // law — so D1 ends at the NEXT appended
    // welcome (whichever wave owns it) and nothing before `d1_start` can
    // have moved.
    assert_eq!(
        frames.len() - d1_start,
        6 + 7 + 3 + 3 + 4 + 3 + 4 + 1,
        "six D1 frames, then T1's seven transcription frames, then U1's \
         three usage frames, then G2's three session-rename frames, then \
         G3's four session-tuning frames, F1's three fleet frames, then \
         WIRE-GAPS' four read frames, then Slice 2's folded response — the \
         accounted tail pins that nothing before d1_start moved"
    );
    for frame in &frames[d1_start..d1_start + 6] {
        let encoded = ws_codec::encode(frame, TEST_FRAME_LIMIT).expect("encode D1 frame");
        assert!(
            !encoded.contains("refresh"),
            "the cut refresh-now action must not ride any D1 frame: {encoded}"
        );
    }

    // Newer-daemon tolerance: unknown extra fields (response-level and
    // candidate-level) are ignored, and the typed shape still lands.
    let future_response = r#"{"v":1,"kind":"response","request_id":"request-future-candidates","body":{"method":"account.device_candidates","discovery_disabled":false,"future_scan_ms":9,"candidates":[{"candidate":"dc1_0000000000000000000000000000000000000000000000000000000000000000","provider":"openai-oauth","source_label":"Codex","freshness":"fresh","path":"/home/future/.codex/auth.json","import_supported":true,"future_field":"ignored"}]}}"#;
    let decoded: WireFrame = serde_json::from_str(future_response).expect("tolerant D1 decode");
    let WireFrame::Response {
        body:
            ResponseBody::AccountDeviceCandidates {
                discovery_disabled: false,
                candidates,
            },
        ..
    } = decoded
    else {
        panic!("expected a typed device-candidates response");
    };
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].account_label, None,
        "absent optional defaults"
    );
    assert_eq!(candidates[0].expires_at_ms, None);
    assert_eq!(candidates[0].unsupported_reason, None);

    // Older-daemon shape: `candidates` itself may be absent; the response
    // still decodes with an empty list rather than failing the frame.
    let minimal_response = r#"{"v":1,"kind":"response","request_id":"request-minimal-candidates","body":{"method":"account.device_candidates","discovery_disabled":true}}"#;
    let decoded: WireFrame = serde_json::from_str(minimal_response).expect("minimal D1 decode");
    assert!(matches!(
        decoded,
        WireFrame::Response {
            body: ResponseBody::AccountDeviceCandidates {
                discovery_disabled: true,
                ref candidates,
            },
            ..
        } if candidates.is_empty()
    ));

    // Encode direction: absent optionals stay OFF the wire (append-only
    // discipline), and no serialized key can carry token material.
    let bare = DeviceCredentialCandidateWire {
        candidate: format!("dc1_{}", "0".repeat(64)),
        provider: "openai-oauth".into(),
        source_label: "Codex".into(),
        account_label: None,
        freshness: "unknown".into(),
        expires_at_ms: None,
        path: "/home/golden/.codex/auth.json".into(),
        import_supported: true,
        unsupported_reason: None,
    };
    let encoded = serde_json::to_value(&bare).expect("encode bare candidate");
    let keys: Vec<&str> = encoded
        .as_object()
        .expect("candidate object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "candidate",
            "freshness",
            "import_supported",
            "path",
            "provider",
            "source_label"
        ],
        "absent optionals must be omitted and no secret-bearing key may exist"
    );
    for key in keys {
        assert!(
            !key.contains("token") && !key.contains("secret") && !key.contains("device_id"),
            "candidate wire grew a secret-shaped field: {key}"
        );
    }

    // The cut refresh-now method never became surface: to every peer it is
    // just another unknown future method, tolerated as Unknown.
    let refresh_request = r#"{"v":1,"kind":"request","request_id":"request-refresh-cut","body":{"method":"account.refresh","alias":"openai-oauth"}}"#;
    let decoded: WireFrame = serde_json::from_str(refresh_request).expect("tolerant decode");
    assert_eq!(
        decoded,
        WireFrame::Request {
            request_id: haider_rpc::RequestId::new("request-refresh-cut"),
            body: RequestBody::Unknown,
        }
    );
}

/// LAW (absent_provider_keeps_legacy_bytes_and_behavior, bytes half): a
/// model-only selection carries NO `provider` key — byte-for-byte the shape
/// a provider-unaware peer would emit — and such a request decodes to
/// `provider: None`.
///
/// MUTATION CHECK: serialize `provider: None` as `"provider":null` or make
/// the field required. Expected RUNTIME failure: the exact golden string or
/// the provider-less decode below.
#[test]
fn session_select_model_absent_provider_keeps_legacy_bytes() {
    let frame = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-select-legacy"),
        body: RequestBody::SessionSelectModel {
            command_id: haider_rpc::CommandId::new("command-select-legacy"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            model: "model-next".into(),
            provider: None,
            confirm_new_epoch: false,
        },
    };
    let encoded = serde_json::to_string(&frame).expect("encode model-only selection");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"request","request_id":"request-select-legacy","body":{"method":"session.select_model","command_id":"command-select-legacy","session_id":"session-1","worker_generation":7,"model":"model-next"}}"#
    );
    let decoded: WireFrame = serde_json::from_str(&encoded).expect("decode model-only selection");
    assert_eq!(decoded, frame);
}

/// The full pair-selection request and its response are golden: the request
/// carries the optional provider attribute of the selected model row, and
/// the response reports the RESOLVED pair plus committed fact coordinates.
#[test]
fn session_select_model_pair_request_and_response_are_golden() {
    let request = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-select-pair"),
        body: RequestBody::SessionSelectModel {
            command_id: haider_rpc::CommandId::new("command-select-pair"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            model: "fable-5".into(),
            provider: Some("anthropic-oauth".into()),
            confirm_new_epoch: false,
        },
    };
    let encoded = serde_json::to_string(&request).expect("encode pair selection");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"request","request_id":"request-select-pair","body":{"method":"session.select_model","command_id":"command-select-pair","session_id":"session-1","worker_generation":7,"model":"fable-5","provider":"anthropic-oauth"}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode pair selection"),
        request
    );

    let response = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("request-select-pair"),
        body: ResponseBody::SessionSelectModel {
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            provider: "anthropic-oauth".into(),
            model: "fable-5".into(),
            selected_seq: 42,
            worker_generation: 7,
        },
    };
    let encoded = serde_json::to_string(&response).expect("encode selection response");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"response","request_id":"request-select-pair","body":{"method":"session.select_model","session_id":"session-1","provider":"anthropic-oauth","model":"fable-5","selected_seq":42,"worker_generation":7}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode selection response"),
        response
    );

    // To an older peer the method is just another tolerated unknown.
    let legacy_view = r#"{"v":1,"kind":"request","request_id":"request-select-pair","body":{"method":"session.select_model","command_id":"c","session_id":"s","worker_generation":1,"model":"m","provider":"p","future_field":true}}"#;
    assert!(serde_json::from_str::<WireFrame>(legacy_view).is_ok());
}

/// LAWS (unavailable_provider_refused_typed /
/// unknown_model_with_known_inventory_refused_typed, wire half): the refusal
/// codes and their typed `ErrorData` kinds are stable, additive vocabulary.
#[test]
fn model_selection_refusals_are_typed_and_golden() {
    assert_eq!(
        [
            haider_rpc::ERROR_CODE_PROVIDER_UNAVAILABLE,
            haider_rpc::ERROR_CODE_MODEL_UNKNOWN,
        ],
        ["provider_unavailable", "model_unknown"]
    );

    let unavailable = serde_json::to_value(ErrorData::ProviderUnavailable {
        provider: "frontier-imaginary".into(),
    })
    .expect("encode provider refusal data");
    assert_eq!(
        unavailable,
        serde_json::json!({
            "kind": "provider_unavailable",
            "provider": "frontier-imaginary",
        })
    );
    let unknown = serde_json::to_value(ErrorData::ModelUnknown {
        provider: "anthropic-oauth".into(),
        model: "fable-9-imaginary".into(),
    })
    .expect("encode model refusal data");
    assert_eq!(
        unknown,
        serde_json::json!({
            "kind": "model_unknown",
            "provider": "anthropic-oauth",
            "model": "fable-9-imaginary",
        })
    );
    // Older peers decode both as tolerated Unknown data, never a hard error.
    assert_eq!(
        serde_json::from_value::<ErrorData>(serde_json::json!({"kind": "model_unknown_v9"}))
            .expect("tolerant decode"),
        ErrorData::Unknown
    );

    // The feature bit is the discovery contract for the whole family.
    assert_eq!(
        haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1,
        "session_model_select_v1"
    );
}

/// LAW (G2 wire): the session-rename family appends exactly three frames at
/// the END of the golden transcript (welcome advertising
/// `session_rename_v1`, the rename request, the committed response); a
/// title-less request (a CLEAR) keeps `title` OFF the wire in both
/// directions; and `SessionSummary.title` is additive — absent for older
/// daemons, tolerated by older readers.
///
/// MUTATION CHECK: serialize `title: null`, rename the `session.rename`
/// method tag, or make `title` required. Expected RUNTIME failure: the
/// exact golden strings or the tolerant decodes below.
#[test]
fn session_rename_frames_are_additive_and_golden() {
    use haider_rpc::{FEATURE_SESSION_RENAME_V1, SessionSummary};

    // The feature bit is a pinned wire literal.
    assert_eq!(FEATURE_SESSION_RENAME_V1, "session_rename_v1");

    // The G2 frames were appended at the then-END of the transcript: three
    // frames, append-only; G3 later appended its four session-tuning frames
    // and F1 its three fleet frames strictly AFTER them (each pinned by its
    // own law).
    let frames = transcript();
    let g2_start = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                WireFrame::Welcome(welcome)
                    if welcome.features.contains(FEATURE_SESSION_RENAME_V1)
            )
        })
        .expect("G2 welcome frame in the golden transcript");
    assert_eq!(
        frames.len() - g2_start,
        3 + 4 + 3 + 4 + 1,
        "G2's three frames, then G3's four tuning frames, F1's three fleet frames, WIRE-GAPS' four read frames, then Slice 2's folded response"
    );

    // Exact golden bytes for the titled request/response pair.
    let request = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-rename"),
        body: RequestBody::SessionRename {
            command_id: haider_rpc::CommandId::new("command-rename"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            title: Some("Parser rewrite".into()),
        },
    };
    let encoded = serde_json::to_string(&request).expect("encode rename request");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"request","request_id":"request-rename","body":{"method":"session.rename","command_id":"command-rename","session_id":"session-1","worker_generation":7,"title":"Parser rewrite"}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode rename request"),
        request
    );

    // A CLEAR keeps `title` OFF the wire entirely (no `title: null`).
    let clear = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-rename-clear"),
        body: RequestBody::SessionRename {
            command_id: haider_rpc::CommandId::new("command-rename-clear"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            title: None,
        },
    };
    let encoded = serde_json::to_string(&clear).expect("encode rename clear");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"request","request_id":"request-rename-clear","body":{"method":"session.rename","command_id":"command-rename-clear","session_id":"session-1","worker_generation":7}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode rename clear"),
        clear
    );

    let response = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("request-rename"),
        body: ResponseBody::SessionRename {
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            title: Some("Parser rewrite".into()),
            renamed_seq: 61,
            worker_generation: 7,
        },
    };
    let encoded = serde_json::to_string(&response).expect("encode rename response");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"response","request_id":"request-rename","body":{"method":"session.rename","session_id":"session-1","title":"Parser rewrite","renamed_seq":61,"worker_generation":7}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode rename response"),
        response
    );

    // To an older peer the method is just another tolerated unknown, and
    // unknown future fields are ignored by a newer one.
    let future_view = r#"{"v":1,"kind":"request","request_id":"request-rename-future","body":{"method":"session.rename","command_id":"c","session_id":"s","worker_generation":1,"title":"t","future_field":true}}"#;
    assert!(serde_json::from_str::<WireFrame>(future_view).is_ok());

    // SessionSummary.title is additive: absent stays OFF the wire, and a
    // summary WITH a title round-trips.
    let bare = serde_json::to_value(SessionSummary {
        session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
        head_seq: 9,
        worker_generation: 7,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        parent_session_id: None,
        kind: None,
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
    })
    .expect("encode bare summary");
    assert!(
        bare.get("title").is_none(),
        "absent title must be omitted: {bare}"
    );
    let titled: SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "session-1",
        "head_seq": 9,
        "worker_generation": 7,
        "title": "Parser rewrite",
    }))
    .expect("decode titled summary");
    assert_eq!(titled.title.as_deref(), Some("Parser rewrite"));
}

/// The T1 transcription-secret family obeys the same additive rules as
/// every v1 method: appended at the transcript END, kind-tagged exact
/// method names, unknown-field tolerance, redacted secrets, and absent
/// optionals kept OFF the wire.
///
/// MUTATION CHECK: rename a `transcription.*` tag, derive `Debug` for the
/// new secret-bearing frames, make `clear` required, or serialize
/// `secret: null` for the empty get. Expected runtime failure: the matching
/// assertion below (tail pin, redaction scan, tolerant decode, or key-set
/// pin).
#[test]
fn transcription_secret_frames_are_additive_and_redacted() {
    // The feature bit is the discovery contract for the family.
    assert_eq!(haider_rpc::FEATURE_TRANSCRIPTION_V1, "transcription_v1");

    // The seven T1 frames sit directly before U1's three usage frames,
    // G2's three session-rename frames, G3's four session-tuning frames, and
    // F1's three fleet frames at the transcript tail (each later wave's own
    // law pins its append).
    let frames = transcript();
    let tail = &frames[frames.len() - 25..frames.len() - 18];
    let methods: Vec<String> = tail
        .iter()
        .map(|frame| {
            let value = serde_json::to_value(frame).expect("frame JSON");
            format!(
                "{}:{}",
                value["kind"].as_str().unwrap_or_default(),
                value["body"]["method"].as_str().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(
        methods,
        vec![
            "request:transcription.secret_set",
            "response:transcription.secret_set",
            "request:transcription.secret_get",
            "response:transcription.secret_get",
            "response:transcription.secret_get",
            "request:transcription.secret_set",
            "response:transcription.secret_set",
        ]
    );

    // Debug redaction: neither direction can reveal the secret.
    let set_request = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-transcription-set"),
        body: RequestBody::TranscriptionSecretSet {
            secret: haider_rpc::SecretWire::new("dg-debug-sentinel-9c8b7a"),
            clear: false,
        },
    };
    let get_response = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("request-transcription-get"),
        body: ResponseBody::TranscriptionSecretGet {
            secret: Some(haider_rpc::SecretWire::new("dg-debug-sentinel-9c8b7a")),
        },
    };
    for frame in [&set_request, &get_response] {
        let debug = format!("{frame:?}");
        assert!(
            !debug.contains("dg-debug-sentinel-9c8b7a"),
            "ordinary frame formatting must never reveal the key: {debug}"
        );
        assert!(
            debug.contains("[REDACTED]"),
            "redaction marker missing: {debug}"
        );
    }

    // Tolerant decode: unknown fields ignored; absent `clear` defaults
    // false (older writers).
    let set_json = format!(
        r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"request","request_id":"r1","body":{{
            "method":"transcription.secret_set","secret":"sk-x","future_set_field":true}}}}"#
    );
    match serde_json::from_str::<WireFrame>(&set_json).expect("tolerant set decode") {
        WireFrame::Request {
            body: RequestBody::TranscriptionSecretSet { clear, .. },
            ..
        } => assert!(!clear, "absent clear defaults to false"),
        other => panic!("expected TranscriptionSecretSet, got {other:?}"),
    }
    let get_json = format!(
        r#"{{"v":{WIRE_PROTOCOL_VERSION},"kind":"request","request_id":"r2","body":{{
            "method":"transcription.secret_get","future_get_field":1}}}}"#
    );
    assert!(matches!(
        serde_json::from_str::<WireFrame>(&get_json).expect("tolerant get decode"),
        WireFrame::Request {
            body: RequestBody::TranscriptionSecretGet,
            ..
        }
    ));

    // Encode direction: the empty get keeps `secret` OFF the wire (no
    // `secret: null` for older readers), and the SET response never carries
    // a secret-bearing key at all.
    let empty = serde_json::to_value(ResponseBody::TranscriptionSecretGet { secret: None })
        .expect("encode empty get");
    assert_eq!(
        empty,
        serde_json::json!({"method": "transcription.secret_get"})
    );
    let set_ok = serde_json::to_value(ResponseBody::TranscriptionSecretSet { present: true })
        .expect("encode set response");
    assert_eq!(
        set_ok,
        serde_json::json!({"method": "transcription.secret_set", "present": true})
    );
}

/// LAW (usage_report_goldens_are_additive_normalized_and_secret_free): the U1
/// wave appends exactly three frames at the END of the golden transcript
/// (welcome advertising `usage_report_v1`, the parameterless request, the
/// report response); utilization rides the wire as the normalized 0–1
/// fraction, never a raw percentage; unknown future fields are tolerated in
/// both directions; and no U1 frame can carry token/key bytes.
/// MUTATION CHECK: serialize a percentage (60 instead of 0.6), rename the
/// method, or drop the tagged meter state. Expected RUNTIME failure: the
/// appended golden bytes differ or the tolerance decode loses its shape.
#[test]
fn usage_report_goldens_are_additive_normalized_and_secret_free() {
    use haider_protocol::usage::AccountMeterStateV1;
    use haider_rpc::FEATURE_USAGE_REPORT_V1;

    // The feature bit is a pinned wire literal.
    assert_eq!(FEATURE_USAGE_REPORT_V1, "usage_report_v1");

    // The U1 frames were appended at the then-END of the transcript: three
    // frames, append-only; G2 later appended three session-rename frames, G3
    // four tuning frames, and F1 three fleet frames strictly AFTER them.
    let frames = transcript();
    let u1_start = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                WireFrame::Welcome(welcome)
                    if welcome.features.contains(FEATURE_USAGE_REPORT_V1)
            )
        })
        .expect("U1 welcome frame in the golden transcript");
    assert_eq!(
        frames.len() - u1_start,
        3 + 3 + 4 + 3 + 4 + 1,
        "three U1 frames, then G2's three session-rename frames, then G3's \
         four session-tuning frames, F1's three fleet frames, then \
         WIRE-GAPS' four read frames, then Slice 2's folded response (each later \
         wave's own law pins its append)"
    );
    for frame in &frames[u1_start..u1_start + 3] {
        let encoded = ws_codec::encode(frame, TEST_FRAME_LIMIT).expect("encode U1 frame");
        // Key positions only: `"api_key"` the AuthMethod VALUE is legitimate;
        // a key named like a secret is not.
        for forbidden in [
            "\"access_token\":",
            "\"refresh_token\":",
            "\"api_key\":",
            "\"secret\":",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "secret-shaped key must not ride a U1 frame: {forbidden}"
            );
        }
    }

    // The response frame carries the normalized fraction — the raw provider
    // percentage (60) must never appear as a utilization value.
    let response = ws_codec::encode(&frames[u1_start + 2], TEST_FRAME_LIMIT).expect("encode");
    assert!(
        response.contains("\"utilization\":0.6"),
        "normalized fraction on the wire: {response}"
    );
    assert!(
        !response.contains("\"utilization\":60"),
        "raw percentage must not ride the wire: {response}"
    );

    // Newer-daemon tolerance: unknown report/entry/window fields are ignored
    // and the typed shape still lands, including the tagged meter state.
    let future_response = r#"{"v":1,"kind":"response","request_id":"request-future-usage","body":{"method":"usage.report","report":{"generated_at_ms":1,"future_total":9,"accounts":[{"provider":"kimi-oauth","alias":"kimi-main","auth_method":"oauth","meter":{"state":"metered","windows":[{"window":"quota","utilization":0.25,"future_scope":"m"}]},"local":{"sessions":0,"total_duration_ms":0,"input_tokens":0,"output_tokens":0},"future_flag":true}]}}}"#;
    let decoded: WireFrame = serde_json::from_str(future_response).expect("tolerant U1 decode");
    let WireFrame::Response {
        body: ResponseBody::UsageReport { report, .. },
        ..
    } = decoded
    else {
        panic!("expected a typed usage.report response");
    };
    assert_eq!(report.accounts.len(), 1);
    let entry = &report.accounts[0];
    assert_eq!(entry.plan, None, "absent optional defaults");
    let AccountMeterStateV1::Metered { windows } = &entry.meter else {
        panic!("expected a metered state");
    };
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].resets_at_ms, None);

    // Older-peer direction: the parameterless request decodes from its bare
    // method object, and an unknown FUTURE method still lands as tolerated
    // Unknown rather than a hard error.
    let bare_request =
        r#"{"v":1,"kind":"request","request_id":"r-1","body":{"method":"usage.report"}}"#;
    let decoded: WireFrame = serde_json::from_str(bare_request).expect("bare U1 request decode");
    assert!(matches!(
        decoded,
        WireFrame::Request {
            body: RequestBody::UsageReport,
            ..
        }
    ));
    let future_request =
        r#"{"v":1,"kind":"request","request_id":"r-2","body":{"method":"usage.report_v9"}}"#;
    let decoded: WireFrame = serde_json::from_str(future_request).expect("future method decode");
    assert!(matches!(
        decoded,
        WireFrame::Request {
            body: RequestBody::Unknown,
            ..
        }
    ));
}

/// LAW (usage_history_wire_preserves_absence): the two read doors are behind
/// one exact feature literal; day grids keep `null` distinct from a present
/// zero slot, range cells keep an absent total distinct from present zero,
/// and meter basis points remain integer bytes.
///
/// MUTATION CHECK: zero-fill the missing slot/range total or serialize meter
/// basis points through a float; the exact Value assertions below fail.
#[test]
fn usage_history_wire_preserves_absence() {
    use haider_protocol::usage::{
        UsageHistoryDailyTotalV1, UsageHistoryDayV1, UsageHistoryMeterSampleV1,
        UsageHistoryRangeDayV1, UsageHistorySlotV1,
    };

    assert_eq!(haider_rpc::FEATURE_USAGE_HISTORY_V1, "usage_history_v1");
    let mut slots = vec![None; 96];
    slots[1] = Some(UsageHistorySlotV1::default());
    let body = ResponseBody::UsageHistoryDay {
        date: "2026-08-24".into(),
        device_id: "dev-0123456789abcdef0123456789abcdef".into(),
        day: Some(UsageHistoryDayV1 {
            date: "2026-08-24".into(),
            device_id: "dev-0123456789abcdef0123456789abcdef".into(),
            backfilled: true,
            keys: Vec::new(),
            slots,
            meter_samples: vec![UsageHistoryMeterSampleV1 {
                account: "work".into(),
                window: "five_hour".into(),
                basis_points: 6_789,
                resets_at_ms: None,
                grace_until_ms: None,
                sampled_at_ms: 1,
                plan: None,
                credits: None,
                hold: None,
                stale: None,
            }],
            version_changes: Vec::new(),
        }),
        availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
    };
    let value = serde_json::to_value(body).expect("encode history day");
    assert_eq!(value["device_id"], "dev-0123456789abcdef0123456789abcdef");
    assert!(value["day"]["slots"][0].is_null());
    assert!(value["day"]["slots"][1].is_object());
    assert_eq!(value["day"]["meter_samples"][0]["basis_points"], 6_789);
    assert!(
        value["day"]["meter_samples"][0]
            .as_object()
            .is_some_and(|sample| !sample.contains_key("credits") && !sample.contains_key("hold")),
        "absent balances must remain absent on the wire"
    );
    let legacy_sample: UsageHistoryMeterSampleV1 = serde_json::from_value(serde_json::json!({
        "account": "work",
        "window": "weekly",
        "basis_points": 0,
        "sampled_at_ms": 1
    }))
    .expect("pre-balance sample decodes");
    assert_eq!(legacy_sample.credits, None);
    assert_eq!(legacy_sample.hold, None);
    let published_zero = UsageHistoryMeterSampleV1 {
        credits: Some(0),
        hold: Some(0),
        ..legacy_sample
    };
    let published_zero = serde_json::to_value(published_zero).expect("published zero encodes");
    assert_eq!(published_zero["credits"], 0);
    assert_eq!(published_zero["hold"], 0);

    let range = ResponseBody::UsageHistoryRange {
        through_date: "2026-08-24".into(),
        device_id: "dev-0123456789abcdef0123456789abcdef".into(),
        days: vec![
            UsageHistoryRangeDayV1 {
                date: "2026-08-23".into(),
                total: None,
            },
            UsageHistoryRangeDayV1 {
                date: "2026-08-24".into(),
                total: Some(UsageHistoryDailyTotalV1::default()),
            },
        ],
        availability: Some(haider_rpc::SnapshotAvailabilityWire::Available),
    };
    let value = serde_json::to_value(range).expect("encode history range");
    assert_eq!(value["device_id"], "dev-0123456789abcdef0123456789abcdef");
    assert!(value["days"][0].get("total").is_none());
    assert!(value["days"][1]["total"].is_object());
}

#[test]
fn computer_permission_action_wire_is_typed_and_url_free() {
    use haider_protocol::permission::SystemPermission;
    use haider_rpc::FEATURE_COMPUTER_PERMISSION_ACTIONS_V1;

    assert_eq!(
        FEATURE_COMPUTER_PERMISSION_ACTIONS_V1,
        "computer_permission_actions_v1"
    );
    let request = RequestBody::ComputerPermissionOpenSettings {
        session_id: haider_protocol::ids::SessionId::new("session-permission"),
        request_id: "grant-screen-1".into(),
        permission: SystemPermission::ScreenRecording,
    };
    assert_eq!(
        serde_json::to_value(&request).expect("request JSON"),
        serde_json::json!({
            "method": "computer.permission_open_settings",
            "session_id": "session-permission",
            "request_id": "grant-screen-1",
            "permission": "screen_recording"
        })
    );
    let response = ResponseBody::ComputerPermissionOpenSettings {
        permission: SystemPermission::ScreenRecording,
    };
    assert_eq!(
        serde_json::to_value(response).expect("response JSON"),
        serde_json::json!({
            "method": "computer.permission_open_settings",
            "permission": "screen_recording"
        })
    );
}

/// G3 goldens: the two session-tuning pairs pin their exact v1 bytes — an
/// effort request/response with the value present, the ABSENT-`effort`
/// revert shape (no `"effort"` key at all, mirroring the select_model
/// absent-provider law), and the fast pair — plus additive-field tolerance
/// and the two feature literals.
///
/// MUTATION CHECK: serialize `effort: None` as `"effort":null`, rename a
/// method tag, or change a feature literal. Expected RUNTIME failure: the
/// exact golden strings below.
#[test]
fn session_tuning_pairs_are_golden_and_revert_omits_the_effort_key() {
    assert_eq!(
        haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1,
        "session_effort_select_v1"
    );
    assert_eq!(
        haider_rpc::FEATURE_SESSION_FAST_SELECT_V1,
        "session_fast_select_v1"
    );
    assert_eq!(
        haider_rpc::ERROR_CODE_EFFORT_UNSUPPORTED,
        "effort_unsupported"
    );
    assert_eq!(haider_rpc::ERROR_CODE_FAST_UNSUPPORTED, "fast_unsupported");

    let request = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-select-effort"),
        body: RequestBody::SessionSelectEffort {
            command_id: haider_rpc::CommandId::new("command-select-effort"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            effort: Some("xhigh".into()),
            confirm_new_epoch: false,
        },
    };
    assert_eq!(
        serde_json::to_string(&request).expect("encode effort request"),
        r#"{"v":1,"kind":"request","request_id":"request-select-effort","body":{"method":"session.select_effort","command_id":"command-select-effort","session_id":"session-1","worker_generation":7,"effort":"xhigh"}}"#
    );

    // The revert carries NO effort key — byte-for-byte what an
    // effort-unaware encoder of the same fields would emit.
    let revert = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-select-effort-revert"),
        body: RequestBody::SessionSelectEffort {
            command_id: haider_rpc::CommandId::new("command-select-effort-revert"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            effort: None,
            confirm_new_epoch: false,
        },
    };
    let encoded = serde_json::to_string(&revert).expect("encode revert");
    assert_eq!(
        encoded,
        r#"{"v":1,"kind":"request","request_id":"request-select-effort-revert","body":{"method":"session.select_effort","command_id":"command-select-effort-revert","session_id":"session-1","worker_generation":7}}"#
    );
    assert_eq!(
        serde_json::from_str::<WireFrame>(&encoded).expect("decode revert"),
        revert
    );

    let response = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("request-select-effort"),
        body: ResponseBody::SessionSelectEffort {
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            effort: Some("xhigh".into()),
            selected_seq: 43,
            worker_generation: 7,
        },
    };
    assert_eq!(
        serde_json::to_string(&response).expect("encode effort response"),
        r#"{"v":1,"kind":"response","request_id":"request-select-effort","body":{"method":"session.select_effort","session_id":"session-1","effort":"xhigh","selected_seq":43,"worker_generation":7}}"#
    );

    let fast = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("request-select-fast"),
        body: RequestBody::SessionSelectFast {
            command_id: haider_rpc::CommandId::new("command-select-fast"),
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            worker_generation: 7,
            enabled: true,
            confirm_new_epoch: false,
        },
    };
    assert_eq!(
        serde_json::to_string(&fast).expect("encode fast request"),
        r#"{"v":1,"kind":"request","request_id":"request-select-fast","body":{"method":"session.select_fast","command_id":"command-select-fast","session_id":"session-1","worker_generation":7,"enabled":true}}"#
    );
    let fast_response = WireFrame::Response {
        request_id: haider_rpc::RequestId::new("request-select-fast"),
        body: ResponseBody::SessionSelectFast {
            session_id: haider_rpc::haider_protocol::ids::SessionId::new("session-1"),
            enabled: true,
            selected_seq: 44,
            worker_generation: 7,
        },
    };
    assert_eq!(
        serde_json::to_string(&fast_response).expect("encode fast response"),
        r#"{"v":1,"kind":"response","request_id":"request-select-fast","body":{"method":"session.select_fast","session_id":"session-1","enabled":true,"selected_seq":44,"worker_generation":7}}"#
    );

    // Additive tolerance: unknown fields ignored, and to an older reader
    // the new methods are just Unknown — never a decode failure.
    let future: RequestBody = serde_json::from_str(
        r#"{"method":"session.select_effort","command_id":"c","session_id":"s","worker_generation":1,"effort":"low","future_field":true}"#,
    )
    .expect("additive effort decode");
    assert!(matches!(future, RequestBody::SessionSelectEffort { .. }));

    // The typed refusal data pins its kind + ladder coordinates.
    let data = ErrorData::EffortUnsupported {
        provider: "anthropic-oauth".into(),
        model: "claude-opus-4-6".into(),
        effort: "xhigh".into(),
        supported: ["low", "medium", "high", "max"].map(str::to_owned).to_vec(),
    };
    assert_eq!(
        serde_json::to_value(&data).expect("effort refusal data"),
        serde_json::json!({
            "kind": "effort_unsupported",
            "provider": "anthropic-oauth",
            "model": "claude-opus-4-6",
            "effort": "xhigh",
            "supported": ["low", "medium", "high", "max"],
        })
    );
    assert_eq!(
        serde_json::to_value(ErrorData::FastUnsupported {
            provider: "anthropic-oauth".into(),
            model: "claude-sonnet-5".into(),
        })
        .expect("fast refusal data"),
        serde_json::json!({
            "kind": "fast_unsupported",
            "provider": "anthropic-oauth",
            "model": "claude-sonnet-5",
        })
    );
}

/// G3 ModelDetailWire additivity: a pre-G3 daemon's detail row (name +
/// context_window only) decodes with EMPTY tuning fields, and a detail row
/// with the tuning unset serializes to the exact pre-G3 bytes — the
/// provider snapshot goldens above stay byte-stable by construction.
///
/// MUTATION CHECK: drop a serde default/skip attribute on the new fields.
/// Expected RUNTIME failure: the legacy decode errors or the bare row's
/// bytes grow a tuning key.
#[test]
fn model_detail_tuning_fields_are_additive_and_skip_empty() {
    let legacy: haider_rpc::ModelDetailWire =
        serde_json::from_str(r#"{"name":"frontier-a","context_window":200000}"#)
            .expect("pre-G3 detail decodes");
    assert!(legacy.supported_efforts.is_empty());
    assert_eq!(legacy.default_effort, None);
    assert!(legacy.supported_speeds.is_empty());
    assert_eq!(legacy.supports_thinking_type, None);
    assert_eq!(
        serde_json::to_string(&legacy).expect("re-encode"),
        r#"{"name":"frontier-a","context_window":200000}"#,
        "unset tuning stays OFF the wire — pre-G3 bytes exactly"
    );

    let tuned: haider_rpc::ModelDetailWire = serde_json::from_str(
        r#"{"name":"claude-opus-5","supported_efforts":["low","max"],"default_effort":"high","supported_speeds":["fast"],"supports_thinking_type":false,"future_field":1}"#,
    )
    .expect("tuned detail decodes with additive tolerance");
    assert_eq!(tuned.supported_efforts, ["low", "max"]);
    assert_eq!(tuned.default_effort.as_deref(), Some("high"));
    assert_eq!(tuned.supported_speeds, ["fast"]);
    assert_eq!(tuned.supports_thinking_type, Some(false));
}

/// FLEET WIRE LAW: the new feature/request/response trio is the exact
/// historical transcript block, keeps protocol v1, and retains the open-enum tolerance
/// used throughout the existing read surfaces.
#[test]
fn session_fleet_frames_are_additive_and_unknown_tolerant() {
    assert_eq!(haider_rpc::FEATURE_SESSION_FLEET_V1, "session_fleet_v1");
    assert_eq!(haider_rpc::FLEET_MAX_NODES, 512);
    let frames = transcript();
    let fleet_start = frames
        .iter()
        .position(|frame| {
            matches!(
                frame,
                WireFrame::Welcome(welcome)
                    if welcome.features.contains(haider_rpc::FEATURE_SESSION_FLEET_V1)
            )
        })
        .expect("fleet feature welcome");
    assert_eq!(frames.len() - fleet_start, 3 + 4 + 1);
    assert!(matches!(
        &frames[fleet_start],
        WireFrame::Welcome(welcome)
            if welcome.features.contains(haider_rpc::FEATURE_SESSION_FLEET_V1)
    ));
    assert!(matches!(
        &frames[fleet_start + 1],
        WireFrame::Request {
            body: RequestBody::SessionFleet { .. },
            ..
        }
    ));
    assert!(matches!(
        &frames[fleet_start + 2],
        WireFrame::Response {
            body: ResponseBody::SessionFleet { .. },
            ..
        }
    ));

    let request = WireFrame::Request {
        request_id: haider_rpc::RequestId::new("fleet-read"),
        body: RequestBody::SessionFleet {
            session_id: haider_protocol::ids::SessionId::new("root-session"),
        },
    };
    assert_eq!(
        serde_json::to_string(&request).expect("encode fleet request"),
        r#"{"v":1,"kind":"request","request_id":"fleet-read","body":{"method":"session.fleet","session_id":"root-session"}}"#
    );

    let unknown_state: haider_rpc::FleetAgentStateWire =
        serde_json::from_str(r#""hibernating""#).expect("future fleet state");
    assert_eq!(unknown_state, haider_rpc::FleetAgentStateWire::Unknown);
    let future_method: RequestBody =
        serde_json::from_str(r#"{"method":"session.fleet_v2","session_id":"root"}"#)
            .expect("future fleet method");
    assert!(matches!(future_method, RequestBody::Unknown));

    #[derive(Debug, Deserialize)]
    struct LegacyFleetNode {
        agent_id: haider_protocol::ids::AgentId,
        #[serde(default)]
        children: Vec<LegacyFleetNode>,
    }

    let legacy_json = r#"{"agent_id":"legacy-agent","session_id":"legacy-session","task":"leaf","depth":1,"parent_session_id":"legacy-root","state":"done","children":[]}"#;
    let legacy: haider_rpc::FleetNodeWire =
        serde_json::from_str(legacy_json).expect("pre-fold-witness node decodes");
    assert_eq!(legacy.folded_children, 0);
    assert!(
        !serde_json::to_string(&legacy)
            .expect("legacy-shaped node re-encodes")
            .contains("folded_children"),
        "zero stays absent so an old peer sees the exact historical shape"
    );

    let mut folded = legacy;
    folded.folded_children = 3;
    let folded_json = serde_json::to_string(&folded).expect("folded node encodes");
    assert!(folded_json.contains(r#""folded_children":3"#));
    let old_decoder: LegacyFleetNode =
        serde_json::from_str(&folded_json).expect("old decoder ignores additive witness");
    assert_eq!(old_decoder.agent_id.as_str(), "legacy-agent");
    assert!(old_decoder.children.is_empty());
}

/// Lineage truth (`session_lineage_v1`) is additive in both directions: an
/// older daemon's summary decodes with `kind`/`parent_session_id` `None`
/// (absence is "unknown", never "root"), a lineage-aware summary encodes
/// snake_case kinds, and a legacy field projection ignores both fields.
///
/// MUTATION CHECK: default a missing `kind` to `Some(Root)`, rename the
/// serde case, or make the decoder strict. Expected failure: the older
/// summary stops reading as unknown, the encode assertions below break,
/// or the legacy decode rejects.
#[test]
fn session_summary_lineage_is_additive_and_old_decoder_tolerant() {
    let child = haider_rpc::SessionSummary {
        session_id: haider_protocol::ids::SessionId::new("session-lineage-child"),
        head_seq: 4,
        worker_generation: 2,
        run_state: None,
        run_id: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        metadata: None,
        provider: None,
        last_model: None,
        cache_lifetime_hit_basis_points: None,
        cache_reread_hit_basis_points: None,
        workspace_cwd: None,
        turn_count: None,
        footprint_tokens: None,
        footprint_truth: None,
        title: None,
        agent_metrics: None,
        parent_session_id: Some(haider_protocol::ids::SessionId::new(
            "session-lineage-parent",
        )),
        kind: Some(haider_rpc::SessionKindWire::Subagent),
        agent_type: None,
        effort: None,
        fast: None,
        account_alias: None,
    };
    let value = serde_json::to_value(&child).expect("encode child summary");
    assert_eq!(value["kind"], "subagent");
    assert_eq!(value["parent_session_id"], "session-lineage-parent");

    let root = haider_rpc::SessionSummary {
        parent_session_id: None,
        kind: Some(haider_rpc::SessionKindWire::Root),
        ..child.clone()
    };
    let value = serde_json::to_value(&root).expect("encode root summary");
    assert_eq!(value["kind"], "root");
    assert_eq!(
        value.get("parent_session_id"),
        None,
        "a root never carries a parent"
    );

    #[derive(Deserialize)]
    struct LegacySummary {
        session_id: haider_protocol::ids::SessionId,
        head_seq: u64,
    }
    let value = serde_json::to_value(&child).expect("encode for legacy");
    let legacy: LegacySummary =
        serde_json::from_value(value).expect("legacy decoder ignores lineage");
    assert_eq!(legacy.session_id.as_str(), "session-lineage-child");
    assert_eq!(legacy.head_seq, 4);

    let older: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "session-lineage-child",
        "head_seq": 4,
        "worker_generation": 2,
    }))
    .expect("older summary decodes");
    assert_eq!(older.kind, None, "absence is unknown, never root");
    assert_eq!(older.parent_session_id, None);
}

/// W-flow additive tolerance for the agent-type binding: request and
/// response decode with unknown additive fields; the summary's
/// `agent_type` is absent for plain sessions and unknown (never plain)
/// from older daemons.
///
/// MUTATION CHECK: rename the wire methods, default a missing summary
/// `agent_type` to `Some(..)`, or make either body strict. Expected
/// failure: a decode below rejects or the absent-field reads change.
#[test]
fn session_select_agent_type_is_additive_and_old_decoder_tolerant() {
    let request = r#"{
        "method":"session.select_agent_type",
        "command_id":"bind-1",
        "session_id":"session-1",
        "worker_generation":3,
        "agent_type":"scout",
        "future_field":true
    }"#;
    let body: RequestBody = serde_json::from_str(request).expect("request decodes");
    let RequestBody::SessionSelectAgentType { agent_type, .. } = body else {
        panic!("expected select_agent_type request");
    };
    assert_eq!(agent_type.as_deref(), Some("scout"));

    let revert = r#"{
        "method":"session.select_agent_type",
        "command_id":"bind-2",
        "session_id":"session-1",
        "worker_generation":3
    }"#;
    let body: RequestBody = serde_json::from_str(revert).expect("revert decodes");
    let RequestBody::SessionSelectAgentType { agent_type, .. } = body else {
        panic!("expected select_agent_type request");
    };
    assert_eq!(agent_type, None, "absence is the revert");

    let response = r#"{
        "method":"session.select_agent_type",
        "session_id":"session-1",
        "agent_type":"scout",
        "selected_seq":9,
        "worker_generation":3
    }"#;
    let body: ResponseBody = serde_json::from_str(response).expect("response decodes");
    let ResponseBody::SessionSelectAgentType {
        agent_type,
        selected_seq,
        ..
    } = body
    else {
        panic!("expected select_agent_type response");
    };
    assert_eq!(agent_type.as_deref(), Some("scout"));
    assert_eq!(selected_seq, 9);

    let older_summary: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "session-1",
        "head_seq": 9,
        "worker_generation": 7,
    }))
    .expect("older summary decodes");
    assert_eq!(older_summary.agent_type, None, "absence is unknown");
}

/// The pipelined-handshake law (v0.0.934 wire fix): one OS read may carry
/// the final JSON handshake frame AND the first MessagePack frame. The
/// decoder must stop at the frame boundary, let the caller switch
/// encodings, and decode the coalesced suffix as MessagePack.
///
/// MUTATION CHECK: make `push_one` consume past the first frame (or make
/// the handshake caller feed the whole chunk through `push` before
/// switching). Expected runtime failure: the MessagePack suffix decodes as
/// JSON and poisons the decoder below.
#[test]
fn a_pipelined_handshake_chunk_switches_encodings_at_the_frame_boundary() {
    use haider_rpc::WireEncoding;
    let hello = WireFrame::Ping { nonce: 1 };
    let event = WireFrame::Pong { nonce: 2 };
    let json_part = uds_codec::encode(&hello, TEST_FRAME_LIMIT).expect("json encode");
    let msgpack_part = uds_codec::encode_with(&event, TEST_FRAME_LIMIT, WireEncoding::MessagePack)
        .expect("msgpack encode");
    let mut chunk = json_part.clone();
    chunk.extend_from_slice(&msgpack_part);

    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let step = decoder.push_one(&chunk);
    assert!(
        step.error.is_none(),
        "first frame decodes: {:?}",
        step.error
    );
    assert!(matches!(step.frame, Some(WireFrame::Ping { nonce: 1 })));
    assert_eq!(
        step.consumed,
        json_part.len(),
        "push_one stops exactly at the frame boundary"
    );
    decoder.set_encoding(WireEncoding::MessagePack);
    assert!(!decoder.is_poisoned(), "a boundary switch is legal");
    let batch = decoder.push(&chunk[step.consumed..]);
    assert!(batch.error.is_none(), "suffix decodes: {:?}", batch.error);
    assert_eq!(batch.frames.len(), 1);
    assert!(matches!(batch.frames[0], WireFrame::Pong { nonce: 2 }));
}

/// The boundary law is a REAL invariant, not a debug assert: switching
/// encodings mid-frame poisons the decoder instead of misdecoding bytes.
#[test]
fn a_mid_frame_encoding_switch_poisons_the_decoder() {
    use haider_rpc::WireEncoding;
    let frame = uds_codec::encode(&WireFrame::Ping { nonce: 1 }, TEST_FRAME_LIMIT).expect("encode");
    let mut decoder = uds_codec::Decoder::new(TEST_FRAME_LIMIT);
    let step = decoder.push_one(&frame[..3]);
    assert!(step.frame.is_none());
    decoder.set_encoding(WireEncoding::MessagePack);
    assert!(
        decoder.is_poisoned(),
        "a mid-frame switch fails closed rather than misdecoding"
    );
}

/// effect_recovery_v1 typed run state is additive: an older client with
/// `#[serde(other)]` decodes `effect_unknown` as Unknown, and a new client
/// decodes every prior value. Roster scalars (effort/fast/account_alias)
/// are additive both directions.
///
/// MUTATION CHECK: rename the serde case, drop the `other` fallback, or make
/// SessionSummary strict. Expected failure: an unknown state fails to decode,
/// or the additive summary rejects.
#[test]
fn effect_recovery_state_and_roster_scalars_are_additive() {
    use haider_rpc::ObserveRunStateWire;
    // New client decodes the new state.
    let state: ObserveRunStateWire =
        serde_json::from_value(serde_json::json!("effect_unknown")).expect("decodes");
    assert!(matches!(state, ObserveRunStateWire::EffectUnknown));
    // A future state an even-newer daemon emits decodes as Unknown, not error.
    let future: ObserveRunStateWire =
        serde_json::from_value(serde_json::json!("some_future_state_v9")).expect("tolerant");
    assert!(matches!(future, ObserveRunStateWire::Unknown));

    // Roster scalars: older daemon (absent) → None; newer → decoded.
    let older: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "s1", "head_seq": 3, "worker_generation": 1,
    }))
    .expect("older summary");
    assert_eq!(older.effort, None);
    assert_eq!(older.fast, None);
    assert_eq!(older.account_alias, None);

    let newer: haider_rpc::SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "s1", "head_seq": 3, "worker_generation": 1,
        "effort": "high", "fast": false, "run_state": "effect_unknown",
        "future_summary_field": true,
    }))
    .expect("newer summary");
    assert_eq!(newer.effort.as_deref(), Some("high"));
    assert_eq!(newer.fast, Some(false), "Some(false) is a real normal mode");
    assert!(matches!(
        newer.run_state,
        Some(ObserveRunStateWire::EffectUnknown)
    ));
    assert_eq!(
        newer.account_alias, None,
        "account slot stays None until the seam ships"
    );
}

/// #16 (935): the borrowed EVENT encode (shared envelope, no logical-frame
/// clone) must produce bytes IDENTICAL to `WireFrame::Event` — prefix and
/// body, in both encodings — or a fan-out attachment would receive
/// different wire bytes than a cloned publish.
///
/// MUTATION CHECK: change the borrowed serializer's shape (field order,
/// tag, version). Expected runtime failure: the borrowed bytes diverge
/// from the owned `WireFrame::Event` encode below.
#[test]
fn borrowed_event_encode_is_byte_identical_to_owned() {
    use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
    use haider_protocol::ids::{DeviceId, EventId, SessionId};
    use haider_rpc::AttachmentId;
    use haider_rpc::WireEncoding;

    let attachment = AttachmentId::new("att-1");
    let session = SessionId::new("s-1");
    let envelope: haider_protocol::envelope::RawEnvelope = EventEnvelope {
        schema_version: 1,
        event_id: EventId::new("e-1"),
        seq: 7,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("d-1"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 42,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({"type": "idle_decayed"}),
    };
    let owned = WireFrame::Event {
        attachment_id: attachment.clone(),
        session_id: session.clone(),
        envelope: envelope.clone(),
    };
    for encoding in [WireEncoding::Json, WireEncoding::MessagePack] {
        let owned_bytes =
            uds_codec::encode_with(&owned, TEST_FRAME_LIMIT, encoding).expect("owned encode");
        let borrowed = uds_codec::encode_event_zeroizing_parts_with(
            &attachment,
            &session,
            &envelope,
            TEST_FRAME_LIMIT,
            encoding,
        )
        .expect("borrowed encode");
        let mut borrowed_bytes = borrowed.prefix().to_vec();
        borrowed_bytes.extend_from_slice(borrowed.body());
        assert_eq!(
            owned_bytes, borrowed_bytes,
            "borrowed EVENT encode must match owned bytes for {encoding:?}"
        );
    }
}

/// Lane S (935): surface attachment refs (input_mirror_attachments_v1) and
/// structured status (status_segment_structured_v1) are additive both ways
/// — an older publisher omitting them decodes with empty/none, and text /
/// line stay the mirror's authoritative value.
///
/// MUTATION CHECK: drop the skip_serializing_if on either field. Expected
/// failure: an empty/none field appears on the wire (breaking byte-identity
/// for existing consumers) or the older-decoder default reads change.
#[test]
fn surface_attachment_refs_and_structured_status_are_additive() {
    use haider_rpc::{SurfaceInputPublishWire, SurfaceStatusPublishWire};

    // Older publisher: text/line only — no attachment or structured keys.
    let older_input = serde_json::to_value(&SurfaceInputPublishWire {
        text: "hi".into(),
        attachments: Vec::new(),
        revision: 3,
    })
    .expect("input encodes");
    assert_eq!(
        older_input.get("attachments"),
        None,
        "empty attachments never ride the wire"
    );
    let older_status = serde_json::to_value(&SurfaceStatusPublishWire {
        line: "[ IDLE ] 0 tok".into(),
        state: None,
        detail: None,
        revision: 3,
    })
    .expect("status encodes");
    assert_eq!(older_status.get("state"), None);
    assert_eq!(older_status.get("detail"), None);
    assert_eq!(older_status["line"], "[ IDLE ] 0 tok");

    // A newer publisher's extra fields decode, and a client that ignores
    // them still reads text/line.
    let newer_input: SurfaceInputPublishWire = serde_json::from_value(serde_json::json!({
        "text": "look",
        "attachments": [{"mime": "image/png", "bytes": 1024, "artifact": "blake3:abc"}],
        "revision": 4,
    }))
    .expect("newer input decodes");
    assert_eq!(newer_input.text, "look", "text stays the mirror truth");
    assert_eq!(newer_input.attachments.len(), 1);
    assert_eq!(newer_input.attachments[0].mime, "image/png");

    let newer_status: SurfaceStatusPublishWire = serde_json::from_value(serde_json::json!({
        "line": "[ RUNNING ] 3k tok",
        "state": "running",
        "detail": "applying patch 3/5",
        "revision": 4,
    }))
    .expect("newer status decodes");
    assert_eq!(
        newer_status.line, "[ RUNNING ] 3k tok",
        "line stays authoritative"
    );
    assert_eq!(newer_status.state.as_deref(), Some("running"));
    assert_eq!(newer_status.detail.as_deref(), Some("applying patch 3/5"));
}

/// v0.0.936 attention state: the summary's `seen_at_ms`/`last_activity_ms`/
/// `waiting_why` are ADDITIVE — absent from older daemons (None, and never
/// serialized when None so pre-936 wire bytes are unchanged) — and
/// `WaitingWhyWire` pins its exact serialized shape.
///
/// MUTATION CHECK (executed): drop a `skip_serializing_if` on the new
/// summary fields and the absent-summary byte pin fails; rename a
/// `WaitingWhyKindWire` variant and the shape golden fails.
#[test]
fn attention_fields_and_waiting_why_are_additive() {
    use haider_rpc::{SessionSummary, WaitingWhyKindWire, WaitingWhyWire};

    // Older daemon: absent fields decode to None.
    let older: SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "s1", "head_seq": 3, "worker_generation": 1,
    }))
    .expect("older summary decodes");
    assert_eq!(older.seen_at_ms, None);
    assert_eq!(older.last_activity_ms, None);
    assert_eq!(older.waiting_why, None);

    // None fields never serialize: pre-936 wire bytes are unchanged.
    let encoded = serde_json::to_value(&older).expect("summary encodes");
    for key in ["seen_at_ms", "last_activity_ms", "waiting_why"] {
        assert!(
            encoded.get(key).is_none(),
            "absent attention field `{key}` must not serialize"
        );
    }

    // Newer daemon: populated fields decode, exact wire shape pinned.
    let newer: SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "s1", "head_seq": 9, "worker_generation": 1,
        "seen_at_ms": 1000, "last_activity_ms": 2000,
        "waiting_why": {"kind": "permission", "pending_menu_id": "menu-7"},
    }))
    .expect("newer summary decodes");
    assert_eq!(newer.seen_at_ms, Some(1000));
    assert_eq!(newer.last_activity_ms, Some(2000));
    let why = newer.waiting_why.expect("waiting_why decodes");
    assert_eq!(why.kind, WaitingWhyKindWire::Permission);

    // The typed shape golden: snake_case kind, optional menu id skipped.
    assert_eq!(
        serde_json::to_value(WaitingWhyWire {
            kind: WaitingWhyKindWire::Approval,
            pending_menu_id: None,
        })
        .expect("encodes"),
        serde_json::json!({"kind": "approval"})
    );
    assert_eq!(
        serde_json::to_value(WaitingWhyWire {
            kind: WaitingWhyKindWire::Question,
            pending_menu_id: Some(haider_protocol::ids::MenuId::new("menu-3")),
        })
        .expect("encodes"),
        serde_json::json!({"kind": "question", "pending_menu_id": "menu-3"})
    );
}

/// v0.0.937 unified input contract: `needs_input` is additive on BOTH the
/// summary and the observe digest, `NeedsInputKindWire` tolerates kinds a
/// newer daemon may add (`#[serde(other)] Unknown` — the arm the frozen
/// 936 `waiting_why` enum lacked), the full card shape is pinned, and the
/// option `decision` rider is additive.
///
/// MUTATION CHECK (executed): remove the `#[serde(other)]` tolerance arm
/// and the future-kind decode fails; drop a `skip_serializing_if` on
/// `needs_input`/`secret_answer` and the byte-stability halves fail.
#[test]
fn needs_input_is_additive_tolerant_and_shape_pinned() {
    use haider_rpc::{NeedsInputKindWire, NeedsInputWire, SessionSummary};

    // Tolerance: an unknown kind decodes to Unknown, never an error.
    let future: NeedsInputKindWire =
        serde_json::from_value(serde_json::json!("holographic_consent_v9")).expect("tolerant");
    assert!(matches!(future, NeedsInputKindWire::Unknown));

    // Older daemon: absent field decodes None and re-encodes with no key.
    let older: SessionSummary = serde_json::from_value(serde_json::json!({
        "session_id": "s1", "head_seq": 3, "worker_generation": 1,
    }))
    .expect("older summary decodes");
    assert_eq!(older.needs_input, None);
    let encoded = serde_json::to_value(&older).expect("encodes");
    assert!(encoded.get("needs_input").is_none());

    // The full card round-trips with its exact wire shape.
    let card = NeedsInputWire {
        kind: NeedsInputKindWire::Permission,
        title: "Allow write?".into(),
        safe_body: vec!["write src/lib.rs".into()],
        menu_id: Some(haider_protocol::ids::MenuId::new("menu-9")),
        request_seq: Some(41),
        worker_generation: Some(2),
        since_ms: Some(1_000),
        options: vec![haider_rpc::ObserveMenuOptionWire {
            key: "approve_once".into(),
            label: "Approve once".into(),
            detail: None,
            decision: Some("allow_once".into()),
        }],
        secret_answer: false,
    };
    assert_eq!(
        serde_json::to_value(&card).expect("encodes"),
        serde_json::json!({
            "kind": "permission",
            "title": "Allow write?",
            "safe_body": ["write src/lib.rs"],
            "menu_id": "menu-9",
            "request_seq": 41,
            "worker_generation": 2,
            "since_ms": 1000,
            "options": [{
                "key": "approve_once",
                "label": "Approve once",
                "decision": "allow_once",
            }],
        }),
        "secret_answer=false and empty optionals never serialize"
    );

    // The minimal badge-only card (parked, no menu) stays tiny.
    let badge = NeedsInputWire {
        kind: NeedsInputKindWire::Recovery,
        title: "Effect outcome unknown".into(),
        safe_body: Vec::new(),
        menu_id: None,
        request_seq: None,
        worker_generation: None,
        since_ms: None,
        options: Vec::new(),
        secret_answer: false,
    };
    assert_eq!(
        serde_json::to_value(&badge).expect("encodes"),
        serde_json::json!({"kind": "recovery", "title": "Effect outcome unknown"})
    );

    // Option decision rider is additive: absent decodes None.
    let old_option: haider_rpc::ObserveMenuOptionWire =
        serde_json::from_value(serde_json::json!({"key": "k", "label": "L"}))
            .expect("older option decodes");
    assert_eq!(old_option.decision, None);

    // DECODE side of the same law (the encode golden above pins only what
    // we WRITE): every omitted field must decode to its default, so the
    // minimal badge card — the exact bytes a parked-without-menu session
    // publishes — round-trips, and `secret_answer` absent reads as FALSE.
    // A client is entitled to treat absence as default; without
    // `serde(default)` on these fields this decode would ERROR instead.
    //
    // MUTATION CHECK (executed): drop `default` from `secret_answer` (or
    // any other omitted field) and this decode fails.
    let minimal: NeedsInputWire = serde_json::from_value(serde_json::json!({
        "kind": "recovery",
        "title": "Effect outcome unknown",
    }))
    .expect("the minimal badge card decodes");
    assert!(!minimal.secret_answer, "absent secret_answer reads false");
    assert!(minimal.safe_body.is_empty() && minimal.options.is_empty());
    assert_eq!(minimal.menu_id, None);
    assert_eq!(minimal.request_seq, None);
    assert_eq!(minimal.worker_generation, None);
    assert_eq!(minimal.since_ms, None);
    // And a card carrying a field a NEWER daemon adds still decodes.
    let forward: NeedsInputWire = serde_json::from_value(serde_json::json!({
        "kind": "permission",
        "title": "Allow?",
        "some_field_from_v999": {"nested": true},
    }))
    .expect("unknown fields are tolerated");
    assert_eq!(forward.kind, NeedsInputKindWire::Permission);
}

/// v0.0.938: the RFC 8628 `user_code` rides `account.oauth_start` ADDITIVELY,
/// so a surface can display the pairing code beside the verification URL
/// instead of parsing it back out of that URL's query string (which is what
/// the ADE had to do). Absent for loopback/PKCE flows — a browser callback
/// carries the grant, there is nothing to type — and absent from older
/// daemons.
///
/// MUTATION CHECK (executed): drop the `skip_serializing_if` and a PKCE start
/// response starts emitting `"user_code": null`, failing the byte-stability
/// half below.
#[test]
fn oauth_start_carries_the_device_user_code_additively() {
    use haider_rpc::{OAuthAvailabilityWire, OAuthFlowId, ResponseBody};

    // A loopback/PKCE start: no user code, and the key must not appear at all.
    let pkce = ResponseBody::AccountOAuthStart {
        availability: OAuthAvailabilityWire {
            available: true,
            reason: None,
        },
        flow_id: Some(OAuthFlowId::new("flow-pkce")),
        authorization_url: None,
        provider_origin: None,
        loopback_port: Some(49_152),
        expires_at_ms: Some(99),
        user_code: None,
    };
    let encoded = serde_json::to_value(&pkce).expect("encodes");
    assert!(
        encoded.get("user_code").is_none(),
        "an absent user code must not serialize: {encoded}"
    );

    // A device start: the code travels beside the URL.
    let device = ResponseBody::AccountOAuthStart {
        availability: OAuthAvailabilityWire {
            available: true,
            reason: None,
        },
        flow_id: Some(OAuthFlowId::new("flow-device")),
        authorization_url: None,
        provider_origin: None,
        loopback_port: None,
        expires_at_ms: Some(99),
        user_code: Some("WDJB-MJHT".into()),
    };
    let encoded = serde_json::to_value(&device).expect("encodes");
    assert_eq!(encoded["user_code"], "WDJB-MJHT");

    // Older daemon: the field is absent and decodes to None, never an error.
    let older: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "account.oauth_start",
        "availability": {"available": true},
        "flow_id": "flow-old",
    }))
    .expect("older start decodes");
    let ResponseBody::AccountOAuthStart { user_code, .. } = older else {
        panic!("expected an oauth start body");
    };
    assert_eq!(user_code, None);
}

/// v0.0.938 `account.list_watch` wire shapes: the request takes no fields,
/// the response is a bare acceptance, and the `AccountsChanged` event carries
/// ONLY the new revision — no descriptors, so the signal can never disagree
/// with the `account.list` snapshot it announces.
///
/// MUTATION CHECK (executed): add descriptors to the event and this shape
/// golden fails — the frame would then be a delta stream that can drift from
/// the snapshot, which is the design this deliberately rejects.
#[test]
fn account_list_watch_is_a_signal_not_a_delta() {
    use haider_rpc::{RequestBody, ResponseBody, WireFrame};

    let request = RequestBody::AccountListWatch {};
    assert_eq!(
        serde_json::to_value(&request).expect("encodes"),
        serde_json::json!({"method": "account.list_watch"}),
        "the watch request carries no parameters"
    );

    let accepted = ResponseBody::AccountListWatch { accepted: true };
    assert_eq!(
        serde_json::to_value(&accepted).expect("encodes"),
        serde_json::json!({"method": "account.list_watch", "accepted": true})
    );

    let event = WireFrame::AccountsChanged { revision: 42 };
    let encoded = serde_json::to_value(&event).expect("encodes");
    assert_eq!(encoded["revision"], 42);
    assert!(
        encoded.get("descriptors").is_none(),
        "the signal carries no registry payload: {encoded}"
    );

    assert_eq!(
        serde_json::from_value::<WireFrame>(encoded).expect("decodes"),
        event,
        "a listed signal must not fall through the owned decoder's Unknown arm"
    );
}

/// The resident binding signal represents unbind by omitting `session_id`,
/// keeps the generation fence required, defaults an old publisher's absent
/// token, and ignores additive future fields. The separate token-echo feature
/// literal lets clients distinguish that additive behavior from the baseline
/// resident-binding frame.
///
/// MUTATION CHECK: replace `binding_token` with `None` in the enclosing
/// `WireFrameOwned::ResidentSessionBinding` conversion. Expected runtime
/// failure: the additive token disappears from the bound round trip while the
/// v0.0.944 tokenless frame continues to decode unchanged.
///
/// MUTATION CHECK: remove `skip_serializing_if = "Option::is_none"` from the
/// resident binding token in `WireFrameRef`. Expected runtime failure: a
/// tokenless publisher serializes `binding_token:null` instead of omitting the
/// field.
#[test]
fn resident_session_binding_decodes_without_optional_fields() {
    use haider_protocol::ids::SessionId;
    use haider_rpc::WireFrame;

    assert_eq!(
        haider_rpc::FEATURE_RESIDENT_SESSION_BINDING_TOKEN_V1,
        "resident_session_binding_token_v1"
    );

    let unbound_json = serde_json::json!({
        "v": 1,
        "kind": "resident_session_binding",
        "worker_generation": 17,
        "future_field": {"safe": true}
    });
    let unbound: WireFrame = serde_json::from_value(unbound_json).expect("unbind decodes");
    assert_eq!(
        unbound,
        WireFrame::ResidentSessionBinding {
            session_id: None,
            worker_generation: 17,
            binding_token: None,
        }
    );
    let unbound_encoded = serde_json::to_value(&unbound).expect("unbind encodes");
    assert!(
        unbound_encoded.get("binding_token").is_none(),
        "a tokenless publisher omits binding_token; it is never null or an empty string"
    );
    assert_eq!(
        unbound_encoded,
        serde_json::json!({
            "v": 1,
            "kind": "resident_session_binding",
            "worker_generation": 17
        })
    );

    let bound = WireFrame::ResidentSessionBinding {
        session_id: Some(SessionId::new("session-bound")),
        worker_generation: 17,
        binding_token: Some("surface-A_7".into()),
    };
    let bound_json = serde_json::to_value(&bound).expect("bound encodes");
    assert_eq!(bound_json["binding_token"], "surface-A_7");
    assert_eq!(
        serde_json::from_value::<WireFrame>(bound_json).expect("bound decodes"),
        bound
    );
}

/// MUTATION CHECK: accept whitespace or more than 128 bytes in
/// `resident_binding_token_is_valid`. Expected runtime failure: an invalid
/// token crosses the protocol's only sanity boundary instead of being
/// rejected as an opaque launch correlator.
#[test]
fn resident_binding_token_sanity_is_bounded_without_interpretation() {
    assert!(haider_rpc::resident_binding_token_is_valid(
        "client-minted.surface_7:hop"
    ));
    assert!(!haider_rpc::resident_binding_token_is_valid(""));
    assert!(!haider_rpc::resident_binding_token_is_valid("has space"));
    assert!(!haider_rpc::resident_binding_token_is_valid(
        &"a".repeat(haider_rpc::RESIDENT_BINDING_TOKEN_MAX_BYTES + 1)
    ));
}

/// MUTATION CHECK: flatten the plan snapshot, rename the feature bit, or
/// derive allowance state from `percent_remaining`. Expected runtime failure:
/// this exact typed frame/feature golden changes and the unknown state no
/// longer survives its round trip verbatim.
#[test]
fn haider_code_plan_status_is_typed_tolerant_and_feature_detectable() {
    use haider_protocol::ids::CredentialAlias;
    use haider_protocol::usage::{HaiderCodeAllowanceStateV1, HaiderCodePlanOutcomeV1};

    assert_eq!(
        haider_rpc::FEATURE_HAIDER_CODE_PLAN_STATUS_V1,
        "haider_code_plan_status_v1"
    );
    let value = serde_json::json!({
        "v": 1,
        "kind": "haider_code_plan_status",
        "provider": "haider-code",
        "account_alias": "haider-primary",
        "outcome": {
            "state": "indeterminate",
            "snapshot": {
                "plan": "go",
                "weekly_allowance": {
                    "percent_remaining": 99.0,
                    "state": "warming",
                    "future_window_field": true
                },
                "refresh_after_s": 37,
                "future_account_field": {"safe": true}
            }
        },
        "future_frame_field": "ignored"
    });
    let frame: WireFrame = serde_json::from_value(value).expect("typed plan frame decodes");
    let WireFrame::HaiderCodePlanStatus {
        provider,
        account_alias,
        outcome: HaiderCodePlanOutcomeV1::Indeterminate { snapshot },
    } = &frame
    else {
        panic!("expected indeterminate Haider Code plan frame");
    };
    assert_eq!(provider, "haider-code");
    assert_eq!(account_alias, &CredentialAlias::new("haider-primary"));
    assert_eq!(snapshot.refresh_after_s, Some(37));
    assert_eq!(
        snapshot
            .weekly_allowance
            .as_ref()
            .and_then(|allowance| allowance.state.as_ref()),
        Some(&HaiderCodeAllowanceStateV1::Unknown("warming".into()))
    );
    assert_eq!(
        serde_json::from_value::<WireFrame>(
            serde_json::to_value(&frame).expect("typed plan frame encodes")
        )
        .expect("typed plan frame round trips"),
        frame
    );

    let future: WireFrame = serde_json::from_value(serde_json::json!({
        "v": 1,
        "kind": "haider_code_plan_status",
        "provider": "haider-code",
        "account_alias": "haider-primary",
        "outcome": {"state": "future_provider_outcome", "new": true}
    }))
    .expect("future outcome degrades");
    assert!(matches!(
        future,
        WireFrame::HaiderCodePlanStatus {
            outcome: HaiderCodePlanOutcomeV1::Unknown,
            ..
        }
    ));
}
