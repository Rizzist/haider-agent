#![allow(clippy::expect_used)]
use haider_protocol::ceiling::{INTERNAL_CEILING_EXIT_CODE, InternalCeilingTerminalV1};

#[test]
fn internal_ceiling_requires_typed_consistent_terminal_evidence() {
    let payload = serde_json::json!({
        "type":"run_state", "state":"errored", "terminal":{
            "end_reason":"harness_internal_ceiling", "internal_cap_detected":true,
            "exit_code":78, "ceilings":{"soft":32,"hard":64,"used":64},
            "continuation":{"session_id":"session", "run_id":"run"},
            "workspace_state":"untouched", "workspace_before":"same", "workspace_after":"same",
            "partial_progress":{"files_written":[],"files_deleted":[],"tool_calls":64,"last_request_ordinal":66}
        }
    });
    let typed = InternalCeilingTerminalV1::from_payload(&payload).expect("typed cap");
    assert_eq!(typed.exit_code, INTERNAL_CEILING_EXIT_CODE);
    assert_eq!(typed.exit_code, 78);
    assert_eq!(
        serde_json::to_value(&typed).expect("serialize"),
        payload["terminal"]
    );
    for (pointer, replacement) in [
        ("/type", serde_json::json!("text")),
        ("/state", serde_json::json!("done")),
        ("/terminal/end_reason", serde_json::json!("provider_error")),
        ("/terminal/internal_cap_detected", serde_json::json!(false)),
        ("/terminal/exit_code", serde_json::json!(70)),
        ("/terminal/ceilings/used", serde_json::json!(32)),
        ("/terminal/ceilings/soft", serde_json::json!(0)),
        ("/terminal/workspace_state", serde_json::json!("unknown")),
        ("/terminal/workspace_after", serde_json::json!("different")),
    ] {
        let mut invalid = payload.clone();
        *invalid.pointer_mut(pointer).expect("field") = replacement;
        assert!(
            InternalCeilingTerminalV1::from_payload(&invalid).is_none(),
            "{pointer}"
        );
    }
    assert!(
        InternalCeilingTerminalV1::from_payload(&serde_json::json!({
            "type":"run_state", "state":"errored", "message":"hard cap reached exit 78"
        }))
        .is_none()
    );
}
