#![allow(clippy::expect_used)]

use haider_protocol::tool::{BoundedResult, ToolResultStatus};

use super::model_tool_result_preview;

fn result(preview: String) -> BoundedResult {
    BoundedResult {
        preview,
        truncated: false,
        data: None,
        artifact: None,
        images: Vec::new(),
        cursor: None,
        status: ToolResultStatus::Completed,
        reason: None,
        presentation: None,
    }
}

#[test]
fn ssh_inventory_is_compacted_only_for_the_model_boundary() {
    let raw = "remote-profile".repeat(1_000);
    let durable = result(raw.clone());
    let (model, truncated) = model_tool_result_preview("ssh_list", &durable);
    assert!(truncated);
    assert!(model.len() <= 8 * 1024);
    assert!(model.contains("SSH profile list compacted for model"));
    assert_eq!(durable.preview, raw, "durable journal input stays raw");
}

#[test]
fn remote_shell_output_is_compacted_only_for_the_model_boundary() {
    let raw = "remote-output".repeat(1_000);
    let durable = result(raw.clone());
    let (model, truncated) = model_tool_result_preview("ssh_shell", &durable);
    assert!(truncated);
    assert!(model.len() <= 8 * 1024);
    assert!(model.contains("remote shell output compacted for model"));
    assert_eq!(durable.preview, raw, "durable journal input stays raw");
}

#[test]
fn profile_targeted_process_output_uses_the_same_model_only_cap() {
    let raw = serde_json::json!({
        "remote": true,
        "stdout": "remote-process-output".repeat(1_000),
    })
    .to_string();
    let durable = result(raw.clone());
    let (model, truncated) = model_tool_result_preview("process_exec", &durable);
    assert!(truncated);
    assert!(model.len() <= 8 * 1024);
    assert!(model.contains("remote process output compacted for model"));
    assert_eq!(durable.preview, raw, "durable journal input stays raw");
}

#[test]
fn local_process_output_keeps_its_existing_model_adapter() {
    let raw = "local-process-output".repeat(1_000);
    let durable = result(raw.clone());
    assert_eq!(
        model_tool_result_preview("process_exec", &durable),
        (raw, false)
    );
}
