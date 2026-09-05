//! Additive v0.0.970 tool-result wire contract. Bless only these fixtures with
//! `UPDATE_FIXTURES=1 cargo test -p haider-protocol --test toolshape_tests`.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::context::elide_text_head_tail;
use haider_protocol::envelope::{
    PromptRender, RawEnvelope, RawPayload, RenderTargets, SCHEMA_VERSION, write_envelope_json,
    write_envelope_messagepack,
};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::tool::{
    BoundedResult, ToolFileEffect, ToolFileEffectKind, ToolResultStatus, ToolTruncation,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::PathBuf;

const ORIGINAL_SHA256: &str = "693cf61417f03d09face28b9d86d3afdfdbaf3f9234b6282ccd5feef62835484";

fn result(preview: impl Into<String>) -> BoundedResult {
    BoundedResult {
        preview: preview.into(),
        truncated: false,
        truncation: None,
        effects: Vec::new(),
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    }
}

fn original_stdout() -> Vec<u8> {
    let mut bytes = vec![b'a'; 1024 * 1024];
    let prefix = b"STDOUT-BEGIN\n";
    let suffix = b"\nSTDOUT-END\n";
    bytes[..prefix.len()].copy_from_slice(prefix);
    let suffix_start = bytes.len() - suffix.len();
    bytes[suffix_start..].copy_from_slice(suffix);
    bytes
}

fn stdout_result() -> BoundedResult {
    let original = original_stdout();
    // Exercise the existing shared prefix/suffix policy, rather than inventing
    // a new projection for the marker. The daemon separately tests its full
    // process-result JSON wrapper and its model-boundary budget.
    let legacy_payload = elide_text_head_tail(
        std::str::from_utf8(&original).expect("ASCII stdout fixture"),
        1024,
        "process_output_byte_cap",
    )
    .expect("1 MiB must exceed the payload cap")
    .text;
    let mut bounded = result(legacy_payload);
    bounded.declare_truncation(ToolTruncation::from_bytes(&original, 0));
    bounded
}

fn fixture_write_result() -> BoundedResult {
    let mut bounded = result("wrote 17 bytes to fixtures/toolshape.txt");
    bounded.effects.push(ToolFileEffect {
        kind: ToolFileEffectKind::Write,
        name: "toolshape.txt".into(),
        path: "fixtures/toolshape.txt".into(),
        absolute_path: "/workspace/fixtures/toolshape.txt".into(),
        bytes: 17,
    });
    bounded
}

fn event(call_id: &str, result: BoundedResult) -> EventPayload {
    EventPayload::ToolResult {
        call_id: call_id.into(),
        result,
    }
}

fn golden<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, value: &T) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/toolshape")
        .join(format!("{name}.json"));
    let actual = serde_json::to_string_pretty(value).expect("serialize toolshape fixture");
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture directory")).expect("mkdir");
        std::fs::write(&path, &actual).expect("bless toolshape fixture");
    }
    let expected = std::fs::read_to_string(path)
        .expect("missing toolshape golden: run this test with UPDATE_FIXTURES=1")
        .replace("\r\n", "\n");
    assert_eq!(actual, expected, "toolshape wire drift: {name}");
    assert_eq!(
        serde_json::from_str::<T>(&expected).expect("decode golden"),
        *value,
        "toolshape round-trip drift: {name}"
    );
}

fn raw_envelope(event: EventPayload) -> RawEnvelope {
    RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("event-toolshape"),
        seq: 61,
        session_id: SessionId::new("session-toolshape"),
        branch_id: None,
        run_id: Some(RunId::new("run-toolshape")),
        agent_id: None,
        device_id: DeviceId::new("device-toolshape"),
        authority_epoch: 2,
        worker_generation: 3,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 4,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: RawPayload::from_event(event).expect("raw tool-result payload"),
    }
}

#[test]
fn one_mib_stdout_golden_preserves_legacy_payload_and_pins_original_digest() {
    let bounded = stdout_result();
    let truncation = bounded.truncation.as_ref().expect("typed truncation");
    assert_eq!(truncation.original_bytes, 1_048_576);
    assert_eq!(truncation.sha256, ORIGINAL_SHA256);
    assert_eq!(
        truncation.payload_bytes,
        bounded.payload_text().len() as u64
    );
    assert!(truncation.truncated);
    assert!(bounded.payload_text().starts_with("STDOUT-BEGIN\n"));
    assert!(bounded.payload_text().ends_with("\nSTDOUT-END\n"));
    let expected_marker = format!(
        "[haider:truncated truncated=true original_bytes=1048576 payload_bytes={} sha256={ORIGINAL_SHA256}]",
        bounded.payload_text().len()
    );
    assert_eq!(
        bounded.preview.lines().last(),
        Some(expected_marker.as_str())
    );
    assert_eq!(bounded.preview.matches("[haider:truncated ").count(), 1);
    assert_eq!(
        bounded.preview,
        format!("{}{expected_marker}", bounded.payload_text()),
        "the original trailing newline already separates the sole final marker"
    );
    golden(
        "stdout_1mib_legacy_payload",
        &bounded.payload_text().to_owned(),
    );
    golden("stdout_1mib_result", &bounded);
    let payload = event("call-stdout-1mib", bounded);
    let json = serde_json::to_value(&payload).expect("event JSON");
    assert_eq!(
        json.pointer("/truncation/sha256"),
        Some(&json!(ORIGINAL_SHA256))
    );
    assert_eq!(
        json.pointer("/truncation"),
        json.pointer("/result/truncation")
    );
    assert!(json.get("effects").is_none());
    golden("stdout_1mib_event", &payload);
}

#[test]
fn digest_changes_when_only_discarded_original_bytes_change() {
    let first = original_stdout();
    let mut second = first.clone();
    second[512 * 1024] = b'b';
    let project = |bytes: &[u8]| {
        elide_text_head_tail(
            std::str::from_utf8(bytes).expect("ASCII fixture"),
            1024,
            "process_output_byte_cap",
        )
        .expect("large stdout")
        .text
    };
    let payload = project(&first);
    assert_eq!(payload, project(&second));
    let first_marker = ToolTruncation::from_bytes(&first, payload.len());
    let second_marker = ToolTruncation::from_bytes(&second, payload.len());
    assert_ne!(first_marker.sha256, second_marker.sha256);
    assert_ne!(
        first_marker.sha256,
        ToolTruncation::from_bytes(payload.as_bytes(), payload.len()).sha256,
        "hashing the retained payload must fail the original-byte contract"
    );
}

#[test]
fn final_marker_is_idempotent_and_counts_payload_bytes_before_separator() {
    for payload in ["", "prefix", "prefix\n", "مرز 😀\n\n"] {
        let mut bounded = result(payload);
        let metadata = ToolTruncation::from_bytes(b"unreduced original", usize::MAX);
        bounded.declare_truncation(metadata.clone());
        let once = bounded.clone();
        bounded.declare_truncation(metadata);
        assert_eq!(bounded, once, "marker duplicated for {payload:?}");
        assert_eq!(bounded.payload_text(), payload);
        assert_eq!(
            bounded
                .truncation
                .as_ref()
                .expect("truncation")
                .payload_bytes,
            payload.len() as u64
        );
        assert_eq!(bounded.preview.matches("[haider:truncated ").count(), 1);
    }
}

#[test]
fn payload_text_does_not_strip_unverified_or_non_utf8_boundary_suffixes() {
    let mut bounded = result("é original payload");
    bounded.declare_truncation(ToolTruncation::from_bytes(b"original", 0));
    let declared = bounded.clone();
    bounded.preview.push_str("\nextra bytes after marker");
    assert_eq!(bounded.payload_text(), bounded.preview);

    bounded = declared.clone();
    bounded.truncation.as_mut().expect("metadata").payload_bytes = 1;
    assert_eq!(
        bounded.payload_text(),
        bounded.preview,
        "never slice inside é"
    );

    bounded = declared;
    bounded.truncation.as_mut().expect("metadata").sha256 = "0".repeat(64);
    assert_eq!(
        bounded.payload_text(),
        bounded.preview,
        "typed and text marker must agree"
    );
}

#[test]
fn fixture_write_golden_exposes_locked_effect_pointers_and_unchanged_text() {
    let bounded = fixture_write_result();
    assert_eq!(bounded.preview, "wrote 17 bytes to fixtures/toolshape.txt");
    assert!(!bounded.truncated);
    assert!(bounded.truncation.is_none());
    golden("fixture_write_result", &bounded);
    let payload = event("call-fixture-write", bounded);
    let json = serde_json::to_value(&payload).expect("event JSON");
    assert_eq!(
        json.pointer("/effects/0/path"),
        Some(&json!("fixtures/toolshape.txt"))
    );
    assert_eq!(
        json.pointer("/effects/0/name"),
        Some(&json!("toolshape.txt"))
    );
    assert_eq!(
        json.pointer("/effects/0/absolute_path"),
        Some(&json!("/workspace/fixtures/toolshape.txt"))
    );
    assert_eq!(json.pointer("/effects/0/bytes"), Some(&json!(17)));
    assert_eq!(json.pointer("/effects"), json.pointer("/result/effects"));
    assert!(json.get("truncation").is_none());
    golden("fixture_write_event", &payload);
}

#[test]
fn all_effect_kinds_keep_receipt_order_instead_of_sorting_paths() {
    let mut bounded = result("four file effects");
    for (kind, path, bytes) in [
        (ToolFileEffectKind::Write, "z-write.txt", 9),
        (ToolFileEffectKind::Create, "a-create.txt", 0),
        (ToolFileEffectKind::Edit, "m-edit.txt", 12),
        (ToolFileEffectKind::Delete, "b-delete.txt", 0),
    ] {
        bounded.effects.push(ToolFileEffect {
            kind,
            name: path.into(),
            path: path.into(),
            absolute_path: format!("/workspace/{path}"),
            bytes,
        });
    }
    let payload = event("call-ordered-effects", bounded);
    let json = serde_json::to_value(&payload).expect("event JSON");
    let kinds = json["effects"]
        .as_array()
        .expect("effects array")
        .iter()
        .map(|effect| effect["kind"].as_str().expect("kind"))
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["write", "create", "edit", "delete"]);
    assert_eq!(json.pointer("/effects/0/path"), Some(&json!("z-write.txt")));
    assert_eq!(
        serde_json::from_value::<EventPayload>(json).expect("decode ordered effects"),
        payload
    );
}

#[test]
fn legacy_result_and_event_are_byte_identical_with_additions_absent() {
    for legacy_result in [
        r#"{"preview":"unchanged","truncated":false}"#,
        r#"{"preview":"historic reduced output","truncated":true}"#,
    ] {
        let bounded: BoundedResult = serde_json::from_str(legacy_result).expect("legacy result");
        assert!(bounded.truncation.is_none());
        assert!(bounded.effects.is_empty());
        assert_eq!(
            serde_json::to_string(&bounded).expect("legacy result JSON"),
            legacy_result
        );
        let legacy_event =
            format!(r#"{{"type":"tool_result","call_id":"call-legacy","result":{legacy_result}}}"#);
        let payload: EventPayload = serde_json::from_str(&legacy_event).expect("legacy event");
        assert_eq!(
            serde_json::to_string(&payload).expect("legacy event JSON"),
            legacy_event
        );
    }
}

#[test]
fn old_decoder_ignores_additive_fields_and_root_only_metadata_decodes() {
    #[derive(Deserialize)]
    struct OldResult {
        preview: String,
        truncated: bool,
    }
    #[derive(Deserialize)]
    struct OldEvent {
        call_id: String,
        result: OldResult,
    }

    for payload in [
        event("call-stdout", stdout_result()),
        event("call-write", fixture_write_result()),
    ] {
        let mut json = serde_json::to_value(&payload).expect("event JSON");
        let old: OldEvent =
            serde_json::from_value(json.clone()).expect("v969 decoder ignores additions");
        assert!(old.call_id.starts_with("call-"));
        assert!(!old.result.preview.is_empty());
        assert_eq!(
            old.result.truncated,
            json["result"]["truncated"].as_bool().expect("bool")
        );
        json["result"]
            .as_object_mut()
            .expect("result object")
            .remove("truncation");
        json["result"]
            .as_object_mut()
            .expect("result object")
            .remove("effects");
        assert_eq!(
            serde_json::from_value::<EventPayload>(json).expect("root metadata decodes"),
            payload
        );
    }
}

#[test]
fn raw_tool_result_json_and_messagepack_live_replay_are_byte_identical() {
    for payload in [
        event("call-stdout", stdout_result()),
        event("call-write", fixture_write_result()),
    ] {
        let expected_payload = payload.clone();
        let envelope = raw_envelope(payload);
        let mut live_json = Vec::new();
        write_envelope_json(&mut live_json, &envelope).expect("live JSON writer");
        assert_eq!(
            live_json,
            serde_json::to_vec(&envelope).expect("reference JSON")
        );
        let replay: RawEnvelope = serde_json::from_slice(&live_json).expect("replay JSON");
        let mut replay_json = Vec::new();
        write_envelope_json(&mut replay_json, &replay).expect("replay JSON writer");
        assert_eq!(live_json, replay_json);
        assert_eq!(
            replay.payload.decode_event().expect("typed replay"),
            expected_payload
        );

        let mut live_messagepack = Vec::new();
        write_envelope_messagepack(&mut live_messagepack, &envelope)
            .expect("live MessagePack writer");
        assert_eq!(
            live_messagepack,
            rmp_serde::to_vec_named(&envelope).expect("reference MessagePack")
        );
        let replay: RawEnvelope =
            rmp_serde::from_slice(&live_messagepack).expect("replay MessagePack");
        let mut replay_messagepack = Vec::new();
        write_envelope_messagepack(&mut replay_messagepack, &replay)
            .expect("replay MessagePack writer");
        assert_eq!(live_messagepack, replay_messagepack);
        assert_eq!(
            replay.payload.decode_event().expect("typed replay"),
            expected_payload
        );
    }
}

#[test]
fn raw_replay_keeps_unknown_additive_fields_at_every_toolshape_level() {
    let mut payload =
        serde_json::to_value(event("call-future", stdout_result())).expect("event JSON");
    payload["future_payload"] = json!({"opaque": [1, 2]});
    payload["truncation"]["future_hash_algorithm"] = json!("sha256");
    payload["result"]["future_result"] = json!({"not_null": true});
    payload["effects"] = json!([{
        "kind": "write", "name": "future.txt", "path": "future.txt",
        "absolute_path": "/workspace/future.txt", "bytes": 1, "future_effect": 42
    }]);
    let mut envelope = raw_envelope(event("unused", result("unused")));
    envelope.payload = RawPayload::Json(payload.clone());
    let live = serde_json::to_vec(&envelope).expect("unknown-field live JSON");
    let replay: RawEnvelope = serde_json::from_slice(&live).expect("unknown-field replay");
    assert_eq!(replay.payload.to_json_value(), payload);
    assert_eq!(
        serde_json::to_vec(&replay).expect("unknown-field replay JSON"),
        live
    );
    assert!(
        replay.payload.decode_event().is_ok(),
        "typed consumers ignore unknown fields"
    );
    let value: Value = serde_json::from_slice(&live).expect("JSON value");
    assert_eq!(
        value.pointer("/payload/effects/0/future_effect"),
        Some(&json!(42))
    );
}
