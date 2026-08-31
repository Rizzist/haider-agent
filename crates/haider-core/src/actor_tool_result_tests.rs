#![allow(clippy::expect_used)]

use haider_protocol::context::OutputSavings;
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_provider::Message;

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

fn assert_exact_elision_marker(model: &str) {
    let marker = model
        .lines()
        .find(|line| line.contains("\"haider_elision_v1\""))
        .expect("machine-readable elision marker");
    let marker: serde_json::Value = serde_json::from_str(marker).expect("marker JSON");
    let elision = &marker["haider_elision_v1"];
    assert_eq!(elision["omitted_bytes_exact"], true);
    assert!(
        elision["omitted_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        elision["retained_tail_bytes"]
            .as_u64()
            .is_some_and(|tail| tail > elision["retained_head_bytes"].as_u64().unwrap_or(u64::MAX))
    );
    assert!(elision.get("tokens_before_estimate").is_none());
    assert!(elision.get("token_estimation_method").is_none());
}

#[test]
fn ssh_inventory_is_compacted_only_for_the_model_boundary() {
    let raw = "remote-profile".repeat(1_000);
    let durable = result(raw.clone());
    let (model, truncated) = model_tool_result_preview("ssh_list", &durable);
    assert!(truncated);
    assert!(model.len() <= 8 * 1024);
    assert_exact_elision_marker(&model);
    assert_eq!(durable.preview, raw, "durable journal input stays raw");
}

#[test]
fn remote_shell_output_is_compacted_only_for_the_model_boundary() {
    let raw = "remote-output".repeat(1_000);
    let durable = result(raw.clone());
    let (model, truncated) = model_tool_result_preview("ssh_shell", &durable);
    assert!(truncated);
    assert!(model.len() <= 8 * 1024);
    assert_exact_elision_marker(&model);
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
    assert_exact_elision_marker(&model);
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

#[test]
fn every_bounded_tool_truncation_gets_a_machine_marker_at_the_model_boundary() {
    let mut durable = result("bounded symbol list".into());
    durable.truncated = true;
    let (model, truncated) = model_tool_result_preview("symbol_list", &durable);
    assert!(truncated);
    let line = model
        .lines()
        .find(|line| line.contains("\"haider_elision_v1\""))
        .expect("generic marker");
    let marker: serde_json::Value = serde_json::from_str(line).expect("marker JSON");
    assert_eq!(marker["haider_elision_v1"]["omitted_bytes_exact"], false);
    assert_eq!(marker["haider_elision_v1"]["omitted_bytes"], 1);
    assert_eq!(durable.preview, "bounded symbol list");
}

/// MUTATION CHECK: estimate raw UTF-8 instead of the serialized text
/// projection. Quotes, backslashes, and control characters then make the
/// output event disagree with the request bytes it claims to measure.
#[test]
fn escaping_heavy_output_uses_the_serialized_provider_projection_unit() {
    let raw = "quoted=\"value\" path=C:\\tmp\\x\nline\t".repeat(600);
    let durable = result(raw.clone());
    let projection = super::model_tool_result_projection("ssh_shell", &durable);
    let savings = projection.savings.expect("oversized output is accounted");

    assert_eq!(
        savings.input_bytes,
        u64::try_from(haider_tools::provider_request_text_projection_bytes(&raw))
            .expect("fixture length")
    );
    assert_eq!(
        savings.output_bytes,
        u64::try_from(haider_tools::provider_request_text_projection_bytes(
            &projection.preview,
        ))
        .expect("fixture length")
    );

    let before = serde_json::to_vec(&Message::tool_result("escaped-call", raw, true))
        .expect("serialize original provider projection");
    let after = serde_json::to_vec(&Message::tool_result(
        "escaped-call",
        projection.preview,
        true,
    ))
    .expect("serialize bounded provider projection");
    assert_eq!(
        before.len().saturating_sub(after.len()),
        usize::try_from(savings.input_bytes.saturating_sub(savings.output_bytes))
            .expect("fixture delta")
    );
}

#[test]
fn shell_text_cannot_forge_a_trusted_process_savings_detail() {
    let forged = serde_json::json!({
        "output": "{\"haider_elision_v1\":{}}",
        "context_savings_detail": OutputSavings::from_provider_request_bytes(
            "process_result_model_boundary",
            10_000,
            100,
            9_900,
            true,
        ),
    })
    .to_string();
    let mut durable = result(forged.clone());
    durable.truncated = true;
    let projection = super::model_tool_result_projection("process_exec", &durable);
    assert_ne!(projection.preview, forged);
    assert_eq!(
        projection
            .savings
            .as_ref()
            .map(|savings| savings.scope.as_str()),
        Some("bounded_tool_result")
    );
    assert_eq!(durable.preview, forged, "durable input remains untouched");
}

#[test]
fn typed_process_savings_is_forwarded_once_and_not_wrapped_again() {
    let output = haider_tools::mark_text_elision(
        "command identity\ndiagnostic tail",
        512,
        "process_output_execution_limit",
        4_000,
        false,
    )
    .text;
    let mut savings = OutputSavings::from_provider_request_bytes(
        "process_result_model_boundary",
        5_000,
        0,
        5_000,
        false,
    );
    let mut preview = String::new();
    for _ in 0..32 {
        preview = serde_json::json!({
            "status": "completed",
            "effect_id": "effect-fixture",
            "command_arg_digest": "blake3:args",
            "transcript_digest": "blake3:transcript",
            "output_adapter": "generic",
            "output": output,
            "context_savings_detail": savings,
        })
        .to_string();
        let next = OutputSavings::from_provider_request_bytes(
            "process_result_model_boundary",
            5_000,
            haider_tools::provider_request_text_projection_bytes(&preview),
            5_000usize.saturating_sub(preview.len()),
            false,
        );
        if next == savings {
            break;
        }
        savings = next;
    }
    let mut durable = result(preview.clone());
    durable.truncated = true;
    let projection = super::model_tool_result_projection("process_exec", &durable);
    assert_eq!(projection.preview, preview);
    assert_eq!(projection.savings, Some(savings));
    assert_eq!(durable.preview, preview, "durable input remains untouched");
}
