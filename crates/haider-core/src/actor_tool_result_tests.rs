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

fn process_envelope(output: &str, exit_code: Option<i32>) -> BoundedResult {
    result(serde_json::json!({
        "status": "completed", "effect_id": "effect-durable-only",
        "command_arg_digest": "blake3:command", "transcript_digest": "blake3:transcript",
        "output_adapter": "generic", "output": output, "exit_code": exit_code,
        "process_signal": {"run_id": "run-durable-only", "call_id": "call", "effect_id": "effect"},
        "limits": {"wall_timeout_ms": 60000, "max_output_bytes": 1048576},
        "workspace_revision": "blake3:workspace", "artifact": null,
    }).to_string())
}

#[test]
fn process_model_envelope_is_output_and_only_nonzero_exit_with_journal_unchanged() {
    for (exit, expected) in [
        (None, "hello\n"),
        (Some(0), "hello\n"),
        (Some(7), "hello\n[exit_code=7]"),
    ] {
        let durable = process_envelope("hello\n", exit);
        let before = serde_json::to_vec(&durable).expect("journal bytes");
        assert_eq!(
            model_tool_result_preview("process_exec", &durable),
            (expected.into(), false)
        );
        assert_eq!(
            model_tool_result_preview("exec", &durable),
            (expected.into(), false)
        );
        let replayed = serde_json::from_slice(&before).expect("replay journal");
        assert_eq!(
            model_tool_result_preview("process_exec", &replayed),
            (expected.into(), false)
        );
        assert_eq!(
            model_tool_result_preview("exec", &replayed),
            (expected.into(), false)
        );
        assert_eq!(serde_json::to_vec(&durable).expect("journal bytes"), before);
        assert!(durable.preview.contains("effect-durable-only"));
        assert!(durable.preview.contains("run-durable-only"));
        assert!(durable.preview.contains("limits"));
    }
}

#[test]
fn process_without_exit_code_retains_terminal_failure_diagnosis() {
    let mut durable = process_envelope("partial output", None);
    durable.status = ToolResultStatus::Failed;
    durable.reason = Some("process ended by signal 9".into());
    assert_eq!(
        model_tool_result_preview("process_exec", &durable).0,
        "partial output\nprocess ended by signal 9"
    );
}

#[test]
fn envelope_shaped_command_output_and_file_contents_are_opaque() {
    let text = process_envelope("do not unwrap this text", Some(9)).preview;
    let durable = process_envelope(&text, Some(0));
    assert_eq!(model_tool_result_preview("process_exec", &durable).0, text);
    let file = result(text.clone());
    assert_eq!(model_tool_result_preview("fs_read", &file).0, text);
}

#[test]
fn filesystem_model_result_drops_receipt_and_preserves_journal_effects() {
    for tool in ["fs_write", "fs_edit", "fs_path", "write", "edit"] {
        let mut durable = result(
            serde_json::json!({
                "result": "wrote src/lib.rs", "mutation_digest": "blake3:mutation",
                "workspace_revision": "blake3:workspace", "subject_digest": "blake3:subject",
                "workspace_mutation": {"run_id": "run-receipt", "effect_id": "effect-receipt"},
            })
            .to_string(),
        );
        durable.effects.push(ToolFileEffect {
            kind: ToolFileEffectKind::Write,
            name: "lib.rs".into(),
            path: "src/lib.rs".into(),
            absolute_path: "/workspace/src/lib.rs".into(),
            bytes: 24,
        });
        let bytes = serde_json::to_vec(&durable).expect("journal");
        assert_eq!(
            model_tool_result_preview(tool, &durable),
            ("wrote src/lib.rs".into(), false)
        );
        let replayed = serde_json::from_slice(&bytes).expect("replay");
        assert_eq!(
            model_tool_result_preview(tool, &replayed).0,
            "wrote src/lib.rs"
        );
        assert_eq!(
            serde_json::to_vec(&durable).expect("unchanged journal"),
            bytes
        );
        let wire: serde_json::Value = serde_json::from_slice(&bytes).expect("journal JSON");
        assert_eq!(wire["effects"][0]["path"], "src/lib.rs");
        assert_eq!(wire["effects"][0]["bytes"], 24);
    }
}

#[test]
fn remote_model_output_keeps_nonzero_exit_and_bounds_after_envelope_removal() {
    let mut durable = result(
        serde_json::json!({
            "remote": true, "untrusted": true, "profile": "machine",
            "stdout": "x".repeat(12000), "stderr": "diagnostic tail", "exit_code": 4,
        })
        .to_string(),
    );
    durable.status = ToolResultStatus::Failed;
    for tool in ["ssh_shell", "process_exec"] {
        let projection = super::model_tool_result_projection(tool, &durable);
        assert!(projection.truncated);
        assert!(projection.preview.len() <= 8192);
        assert!(
            projection
                .preview
                .ends_with("diagnostic tail\n[exit_code=4]")
        );
        assert!(!projection.preview.contains("\"profile\""));
    }
}

#[test]
fn stale_read_digests_and_graph_receipt_fingerprints_stay_journal_only() {
    let mut stale = result(serde_json::json!({"status":"rejected", "error":{
        "kind":"stale_read", "message":"re-read before editing", "details":{
            "current_digest":"blake3:new", "recorded_digest":"blake3:old", "remedy":"re-read before editing"}}}).to_string());
    stale.status = ToolResultStatus::Rejected;
    let model = model_tool_result_preview("fs_edit", &stale).0;
    assert!(model.contains("stale_read") && model.contains("re-read before editing"));
    assert!(!model.contains("blake3:"));
    assert!(stale.preview.contains("blake3:new"));
    let receipt = result(
        serde_json::json!({"ok":true, "node":"VERIFY", "attempt":1,
        "graph_id":"graph", "fingerprint":"blake3:receipt", "through_seq":21})
        .to_string(),
    );
    let model: serde_json::Value =
        serde_json::from_str(&model_tool_result_preview("graph_evidence", &receipt).0)
            .expect("graph output");
    assert_eq!(
        model,
        serde_json::json!({"ok":true, "node":"VERIFY", "attempt":1, "graph_id":"graph"})
    );
    assert!(receipt.preview.contains("fingerprint"));
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
fn typed_process_savings_is_journal_only_and_recosted_for_model_output() {
    let (preview, savings) = process_savings_preview();
    let mut durable = result(preview.clone());
    durable.truncated = true;
    let projection = super::model_tool_result_projection("process_exec", &durable);
    let output: serde_json::Value = serde_json::from_str(&preview).expect("process envelope");
    assert_eq!(
        projection.preview,
        output["output"].as_str().expect("output")
    );
    let projected_savings = projection.savings.expect("journal output accounting");
    assert!(projected_savings.output_bytes < savings.output_bytes);
    assert_savings_disclosures_unchanged(&savings, &projected_savings);
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
    assert!(typed_savings.output_bytes < legacy_savings.output_bytes);
    assert_savings_disclosures_unchanged(&legacy_savings, &typed_savings);
    assert_eq!(
        projection.preview,
        expected_declared_preview(
            serde_json::from_str::<serde_json::Value>(&preview).expect("process envelope")["output"]
                .as_str().expect("output"),
            durable.truncation.as_ref().expect("provenance"))
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
