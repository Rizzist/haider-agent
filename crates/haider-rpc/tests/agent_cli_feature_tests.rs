#![allow(clippy::expect_used)]

use haider_rpc::{FEATURE_AGENT_CLI_V1, RequestBody};
use serde_json::json;

/// MUTATION CHECK: ignoring the additive pin on an older daemon must never
/// silently convert an operator spawn into an ordinary parent provider turn.
#[test]
fn operator_spawn_requires_feature_while_legacy_headless_shape_is_unchanged() {
    let mut wire = json!({
        "method":"headless.run.start", "command_id":"spawn-command",
        "session_id":"parent", "worker_generation":1, "text":"work",
        "attachments":[], "trust_hooks":false,
        "spec":{"provider":"fake","model":"fake-model","max_output_tokens":64,"fast":false}
    });
    let legacy: RequestBody = serde_json::from_value(wire.clone()).expect("legacy request");
    assert_eq!(legacy.additive_shape_feature(), None);
    assert_eq!(
        serde_json::to_value(&legacy).expect("legacy encoding"),
        wire
    );
    wire["spec"]["agent_spawn"] = json!({"task":"task","prompt":"work"});
    let spawn: RequestBody = serde_json::from_value(wire.clone()).expect("public spawn request");
    assert_eq!(FEATURE_AGENT_CLI_V1, "agent_cli_v1");
    assert_eq!(spawn.additive_shape_feature(), Some(FEATURE_AGENT_CLI_V1));
    assert_eq!(serde_json::to_value(&spawn).expect("spawn encoding"), wire);
}
