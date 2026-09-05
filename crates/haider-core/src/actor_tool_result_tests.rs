#![allow(clippy::expect_used)]

use haider_protocol::context::OutputSavings;
use haider_protocol::tool::{
    BoundedResult, ToolFileEffect, ToolFileEffectKind, ToolResultStatus, ToolTruncation,
};
use haider_provider::Message;

use super::model_tool_result_preview;

fn result(preview: String) -> BoundedResult {
    BoundedResult {
        preview,
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

fn process_savings_preview() -> (String, OutputSavings) {
    let output = haider_tools::mark_text_elision(
        "command identity\ndiagnostic tail",
        512,
        "process_output_execution_limit",
        4_000,
        false,
    )
    .text;
    let disclosures = |mut savings: OutputSavings| {
        savings.omitted_items_at_least = Some(7);
        savings.omitted_item_unit = Some("lines".into());
        savings.source_item_id = Some("item-process-fixture".into());
        savings.source_omitted_bytes_at_least = Some(4_000);
        savings.retained_head_bytes = Some(8);
        savings.retained_tail_bytes = Some(24);
        savings
    };
    let mut savings = disclosures(OutputSavings::from_provider_request_bytes(
        "process_result_model_boundary",
        5_000,
        0,
        5_000,
        false,
    ));
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
        let next = disclosures(OutputSavings::from_provider_request_bytes(
            "process_result_model_boundary",
            5_000,
            haider_tools::provider_request_text_projection_bytes(&preview),
            5_000usize.saturating_sub(preview.len()),
            false,
        ));
        if next == savings {
            break;
        }
        savings = next;
    }
    (preview, savings)
}

#[test]
fn typed_process_savings_is_forwarded_once_and_not_wrapped_again() {
    let (preview, savings) = process_savings_preview();
    let mut durable = result(preview.clone());
    durable.truncated = true;
    let projection = super::model_tool_result_projection("process_exec", &durable);
    assert_eq!(projection.preview, preview);
    assert_eq!(projection.savings, Some(savings));
    assert_eq!(durable.preview, preview, "durable input remains untouched");
}

/// Verifier V5 regression: a footer has a provider cost, but it does not put
/// previously omitted source bytes or items back into the result.
#[test]
fn declared_process_footer_changes_only_cost_and_preserves_omission_disclosures() {
    let (preview, legacy_savings) = process_savings_preview();
    assert!(legacy_savings.omitted_bytes > 0);
    assert!(legacy_savings.omitted_items_at_least.is_some());
    assert!(legacy_savings.omitted_item_unit.is_some());
    assert!(legacy_savings.source_item_id.is_some());
    assert!(legacy_savings.source_omitted_bytes_at_least.is_some());
    assert!(legacy_savings.retained_head_bytes.is_some());
    assert!(legacy_savings.retained_tail_bytes.is_some());
    let mut durable = result(preview.clone());
    durable.declare_truncation(ToolTruncation::from_bytes(&[b'x'; 5_000], 0));
    let projection = super::model_tool_result_projection("process_exec", &durable);
    let typed_savings = projection.savings.expect("declared process accounting");
    assert!(typed_savings.output_bytes > legacy_savings.output_bytes);
    assert_savings_disclosures_unchanged(&legacy_savings, &typed_savings);
    assert_eq!(
        projection.preview,
        expected_declared_preview(&preview, durable.truncation.as_ref().expect("provenance"))
    );
    assert_eq!(durable.payload_text(), preview);
}

#[test]
fn typed_filesystem_truncation_preserves_savings_and_first_send_replay_footer_bytes() {
    let original = "quoted=\"value\" path=C:\\tmp\\x\nline\t".repeat(600);
    let payload = haider_tools::elide_text_head_tail(&original, 1024, "filesystem_result")
        .expect("large filesystem result")
        .text;
    for tool in ["fs_read", "fs_search", "fs_glob"] {
        let mut legacy = result(payload.clone());
        legacy.truncated = true;
        let legacy_projection = super::model_tool_result_projection(tool, &legacy);
        let legacy_savings = legacy_projection.savings.expect("legacy accounting");
        let mut durable = legacy;
        durable.declare_truncation(ToolTruncation::from_bytes(original.as_bytes(), 0));
        let expected_model = expected_declared_preview(
            &legacy_projection.preview,
            durable.truncation.as_ref().expect("durable provenance"),
        );
        let projection = super::model_tool_result_projection(tool, &durable);
        let savings = projection
            .savings
            .expect("typed filesystem output still accounts savings");
        assert_eq!(savings.scope, "bounded_tool_result");
        assert_savings_disclosures_unchanged(&legacy_savings, &savings);
        assert_eq!(savings.input_bytes, legacy_savings.input_bytes);
        assert_eq!(
            savings.estimated_tokens_before,
            legacy_savings.estimated_tokens_before
        );
        assert_eq!(
            savings.output_bytes,
            haider_tools::provider_request_text_projection_bytes(&expected_model) as u64,
            "the footer's escaped provider bytes count once"
        );
        assert_eq!(projection.preview, expected_model);
        assert_eq!(durable.payload_text(), payload);
        assert_eq!(projection.preview.matches("[haider:truncated ").count(), 1);
        assert!(projection.truncated);

        let persisted = serde_json::to_vec(&durable).expect("persist bounded result");
        let replayed: BoundedResult =
            serde_json::from_slice(&persisted).expect("replay bounded result");
        let replay_projection = super::model_tool_result_projection(tool, &replayed);
        assert_eq!(replay_projection.preview, projection.preview);
        assert_eq!(replay_projection.savings, Some(savings));
        let live_message = Message::tool_result("call-typed-filesystem", projection.preview, true);
        let replay_message =
            Message::tool_result("call-typed-filesystem", replay_projection.preview, true);
        assert_eq!(
            serde_json::to_vec(&live_message).expect("first-send provider message"),
            serde_json::to_vec(&replay_message).expect("replayed provider message")
        );
    }
}

#[test]
fn typed_filesystem_text_cannot_impersonate_trusted_process_savings() {
    let (preview, process_savings) = process_savings_preview();
    assert_eq!(
        super::trusted_process_output_savings(&preview),
        Some(process_savings)
    );
    let mut legacy = result(preview.clone());
    legacy.truncated = true;
    let legacy_projection = super::model_tool_result_projection("fs_read", &legacy);
    let mut durable = legacy;
    durable.declare_truncation(ToolTruncation::from_bytes(preview.as_bytes(), 0));
    let projection = super::model_tool_result_projection("fs_read", &durable);
    assert_eq!(
        projection
            .savings
            .expect("generic filesystem accounting")
            .scope,
        "bounded_tool_result",
        "process-shaped file content has no authority to declare process savings"
    );
    assert_eq!(
        projection.preview,
        expected_declared_preview(
            &legacy_projection.preview,
            durable.truncation.as_ref().expect("durable provenance")
        )
    );
    assert_eq!(durable.payload_text(), preview);
}

fn expected_declared_preview(payload: &str, provenance: &ToolTruncation) -> String {
    let separator = if payload.is_empty() || payload.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!(
        "{payload}{separator}[haider:truncated truncated=true original_bytes={} payload_bytes={} sha256={}]",
        provenance.original_bytes,
        payload.len(),
        provenance.sha256,
    )
}

fn assert_savings_disclosures_unchanged(legacy: &OutputSavings, typed: &OutputSavings) {
    assert_eq!(typed.omitted_bytes, legacy.omitted_bytes);
    assert_eq!(typed.retained_head_bytes, legacy.retained_head_bytes);
    assert_eq!(typed.retained_tail_bytes, legacy.retained_tail_bytes);
    let mut normalized = typed.clone();
    normalized.output_bytes = legacy.output_bytes;
    normalized.estimated_tokens_after = legacy.estimated_tokens_after;
    normalized.estimated_net_tokens_saved = legacy.estimated_net_tokens_saved;
    normalized.estimated_tokens_saved = legacy.estimated_tokens_saved;
    assert_eq!(
        &normalized, legacy,
        "only the four provider output-cost counters may change for the footer"
    );
}

/// Verifier V4 regression: a declared source marker must not bypass the
/// pre-existing 8 KiB provider cap for a larger producer-side preview.
#[test]
fn typed_large_tool_results_keep_legacy_model_prefix_suffix_cap_and_replay_bytes() {
    const MODEL_PAYLOAD_CAP: usize = 8 * 1024;
    let source = format!("MODEL-HEAD\n{}\nMODEL-TAIL", "x".repeat(96 * 1024));
    for tool in ["fs_read", "web_fetch", "task_output"] {
        let mut legacy = result(source.clone());
        legacy.truncated = true;
        let legacy_projection = super::model_tool_result_projection(tool, &legacy);
        assert!(legacy_projection.preview.len() <= MODEL_PAYLOAD_CAP);
        assert!(legacy_projection.preview.starts_with("MODEL-HEAD\n"));
        assert!(legacy_projection.preview.ends_with("\nMODEL-TAIL"));

        let mut durable = legacy;
        durable.declare_truncation(ToolTruncation::from_bytes(source.as_bytes(), 0));
        let durable_bytes = serde_json::to_vec(&durable).expect("durable large tool result");
        let provenance = durable
            .truncation
            .as_ref()
            .expect("durable source provenance");
        let expected_model = expected_declared_preview(&legacy_projection.preview, provenance);
        let projection = super::model_tool_result_projection(tool, &durable);
        assert_savings_disclosures_unchanged(
            legacy_projection.savings.as_ref().expect("legacy savings"),
            projection.savings.as_ref().expect("declared savings"),
        );
        assert_eq!(
            projection.preview, expected_model,
            "legacy payload plus only the declared footer"
        );
        assert_ne!(
            projection.preview, durable.preview,
            "the provider payload still applies its smaller cap"
        );
        let footer = projection
            .preview
            .lines()
            .last()
            .expect("declared final line");
        assert!(projection.preview.len() <= MODEL_PAYLOAD_CAP + 1 + footer.len());
        assert!(footer.contains(&format!(
            " payload_bytes={} ",
            legacy_projection.preview.len()
        )));
        assert_eq!(projection.preview.matches("[haider:truncated ").count(), 1);
        assert_eq!(durable.payload_text(), source);
        assert_eq!(
            serde_json::to_vec(&durable).expect("unchanged durable result"),
            durable_bytes
        );

        let replayed: BoundedResult =
            serde_json::from_slice(&durable_bytes).expect("replayed result");
        let replay_projection = super::model_tool_result_projection(tool, &replayed);
        assert_eq!(replay_projection.preview, projection.preview);
        assert_eq!(replay_projection.savings, projection.savings);
        let live_message = Message::tool_result("call-large-typed", projection.preview, true);
        let replay_message =
            Message::tool_result("call-large-typed", replay_projection.preview, true);
        assert_eq!(
            serde_json::to_vec(&live_message).expect("live provider bytes"),
            serde_json::to_vec(&replay_message).expect("replayed provider bytes")
        );
    }
}

/// Upstream tool-name repair must transform only the payload, then redeclare
/// the original provenance outside its JSON wrapper with the new byte count.
#[test]
fn repaired_tool_name_preserves_declared_json_and_text_result_provenance() {
    let tool = super::ToolAccumulator {
        item_id: haider_protocol::ids::ItemId::new("item-repaired-toolshape"),
        call_id: "call-repaired-toolshape".into(),
        name: "fs_read".into(),
        args: "{}".into(),
        requested_name: Some("FsRead".into()),
        parsed_args: std::sync::OnceLock::new(),
    };
    for payload in [
        r#"{"output":"prefix and tail"}"#,
        "plain \"prefix\"\nمرز tail",
    ] {
        let mut bounded = result(payload.into());
        bounded.effects.push(ToolFileEffect {
            kind: ToolFileEffectKind::Write,
            name: "fixture.txt".into(),
            path: "fixtures/fixture.txt".into(),
            absolute_path: "/workspace/fixtures/fixture.txt".into(),
            bytes: 7,
        });
        let legacy_corrected = tool.correct_result(bounded.clone());
        bounded.declare_truncation(ToolTruncation::from_bytes(&[b'x'; 10_000], 0));
        let original_provenance = bounded.truncation.clone().expect("original provenance");
        let corrected = tool.correct_result(bounded.clone());
        let provenance = corrected.truncation.as_ref().expect("corrected provenance");
        assert!(corrected.truncated);
        assert_eq!(
            provenance.original_bytes,
            original_provenance.original_bytes
        );
        assert_eq!(provenance.sha256, original_provenance.sha256);
        assert_eq!(
            provenance.payload_bytes,
            legacy_corrected.preview.len() as u64
        );
        assert_ne!(provenance.payload_bytes, original_provenance.payload_bytes);
        assert_eq!(corrected.payload_text(), legacy_corrected.preview);
        assert_eq!(
            corrected.preview,
            expected_declared_preview(&legacy_corrected.preview, &original_provenance)
        );
        assert_eq!(corrected.preview.matches("[haider:truncated ").count(), 1);
        assert_eq!(corrected.effects, bounded.effects);
        let corrected_json: serde_json::Value =
            serde_json::from_str(corrected.payload_text()).expect("corrected payload JSON");
        assert_eq!(
            corrected_json["tool_name_correction"],
            serde_json::json!({"requested": "FsRead", "resolved": "fs_read"})
        );
        if let Ok(original_json) = serde_json::from_str::<serde_json::Value>(payload) {
            assert_eq!(corrected_json["output"], original_json["output"]);
            assert!(
                corrected_json.get("result").is_none(),
                "JSON stays structured"
            );
        } else {
            assert_eq!(corrected_json["result"], payload);
        }
        assert!(!corrected.payload_text().contains("[haider:truncated "));
    }
}
