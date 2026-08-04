#![allow(clippy::expect_used)]

use haider_protocol::effect::EffectClass;
use haider_protocol::ids::AgentId;
use haider_protocol::tool::DispatchMode;
use haider_tools::{MessageSubagent, message_subagent_manifest, spawn_subagent_manifest};

/// MUTATION CHECK: make messaging deferred/effectful, route by display text,
/// or remove either handoff advertisement. Expected RUNTIME failure: the
/// canonical manifests no longer describe the shared delivery/path contract.
#[test]
fn message_manifest_is_awaited_effectless_and_advertises_ephemeral_handoff() {
    let manifest = message_subagent_manifest();
    assert_eq!(manifest.name, "message_subagent");
    assert_eq!(manifest.dispatch, DispatchMode::Await);
    assert_eq!(manifest.effects, Vec::<EffectClass>::new());
    assert_eq!(
        manifest.input_schema["required"],
        serde_json::json!(["agent", "message"])
    );
    for description in [manifest.description, spawn_subagent_manifest().description] {
        assert!(description.contains("EPHEMERAL"));
        assert!(description.contains("<workspace>/.haider/handoff/<session-short>/"));
    }
}

/// MUTATION CHECK: stop trimming/bounding message bytes or accept an empty
/// opaque target. Expected RUNTIME failure: a degenerate message crosses the
/// child-delivery boundary or the valid fixture loses exact normalized text.
#[test]
fn message_arguments_are_opaque_trimmed_and_bounded() {
    let request = MessageSubagent::from_tool_args(serde_json::json!({
        "agent": "agent-f4d8",
        "message": "  inspect src/parser.rs and report the failing branch  "
    }))
    .expect("valid child message");
    assert_eq!(request.agent, AgentId::new("agent-f4d8"));
    assert_eq!(
        request.message,
        "inspect src/parser.rs and report the failing branch"
    );
    assert!(
        MessageSubagent::from_tool_args(serde_json::json!({
            "agent": "",
            "message": "work"
        }))
        .is_err()
    );
    assert!(
        MessageSubagent::from_tool_args(serde_json::json!({
            "agent": "agent-f4d8",
            "message": "x".repeat(32 * 1024 + 1)
        }))
        .is_err()
    );
}
