#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::context::{
    ContextCompactionTier, ContextEconomy, ContextFootprint, ContextFootprintTruth,
    ContextSavingsEvent, ContextSavingsLayer, ContextSavingsMeasurement, OutputSavings,
};

#[test]
fn cumulative_savings_are_saturating_restart_coordinates_with_an_honest_unit() {
    let (first, structural) = ContextEconomy::default().record_with_removed_tool_calls(
        ContextCompactionTier::StructuralTrim24,
        100,
        60,
        vec!["call-a".into(), "call-b".into()],
    );
    let (second, summary) = first.record(ContextCompactionTier::Summarize, 60, 10);

    assert_eq!(structural.estimated_tokens_saved, 40);
    assert_eq!(summary.estimated_tokens_saved, 50);
    assert_eq!(second.cumulative_estimated_tokens_saved, 90);
    assert_eq!(second.operation_count, 2);
    assert_eq!(summary.session_cumulative_estimated_tokens_saved, 90);
    assert_eq!(summary.session_operation_count, 2);
    assert_eq!(
        summary.measurement,
        ContextSavingsMeasurement::ProviderRequestBytesDivFourV1
    );
    assert_eq!(structural.layer, ContextSavingsLayer::Conversation);
    assert_eq!(
        structural.tier,
        Some(ContextCompactionTier::StructuralTrim24)
    );
    assert_eq!(structural.removed_tool_call_ids, ["call-a", "call-b"]);

    let carrier = structural.extension_item().expect("encode savings event");
    assert_eq!(
        ContextSavingsEvent::from_extension_item(&carrier),
        Some(structural)
    );
}

#[test]
fn output_then_conversation_savings_telescope_without_double_counting() {
    let output = OutputSavings::from_provider_request_bytes(
        "fixture_tool_output",
        4_000,
        2_400,
        1_600,
        true,
    );
    let (after_output, output_event) = ContextEconomy::default().record_tool_output(output);
    let (after_compaction, compaction_event) = after_output.record(
        ContextCompactionTier::Summarize,
        output_event.estimated_tokens_after,
        100,
    );

    assert_eq!(output_event.estimated_tokens_before, 1_000);
    assert_eq!(output_event.estimated_tokens_after, 600);
    assert_eq!(output_event.estimated_tokens_saved, 400);
    assert_eq!(compaction_event.estimated_tokens_before, 600);
    assert_eq!(compaction_event.estimated_tokens_after, 100);
    assert_eq!(compaction_event.estimated_tokens_saved, 500);
    assert_eq!(after_compaction.cumulative_estimated_tokens_saved, 900);
    assert_eq!(
        output_event
            .estimated_tokens_before
            .saturating_sub(compaction_event.estimated_tokens_after),
        after_compaction.cumulative_estimated_tokens_saved
    );
}

#[test]
fn ctx_era_conversation_event_without_layer_remains_decodable() {
    let legacy = serde_json::json!({
        "tier": "summarize",
        "estimated_tokens_before": 1_000,
        "estimated_tokens_after": 100,
        "estimated_tokens_saved": 900,
        "session_cumulative_estimated_tokens_saved": 900,
        "session_operation_count": 1,
        "measurement": "provider_request_bytes_div_four_v1",
        "removed_tool_call_ids": [],
    });
    let event: ContextSavingsEvent =
        serde_json::from_value(legacy).expect("ctx-era savings remain additive");
    assert_eq!(event.layer, ContextSavingsLayer::Conversation);
    assert_eq!(
        event.conversation(),
        Some((ContextCompactionTier::Summarize, [].as_slice()))
    );
}

/// Exact old-shape decoder: ctx-v1 required `tier` inside `last_event` and
/// ignored additive fields. An output operation must never replace that slot
/// with its tier-less child payload.
#[test]
fn ctx_era_economy_decoder_ignores_output_child_without_choking() {
    #[derive(serde::Deserialize)]
    struct CtxEraEconomy {
        last_event: Option<CtxEraEvent>,
    }

    #[derive(serde::Deserialize)]
    struct CtxEraEvent {
        tier: ContextCompactionTier,
    }

    let (conversation, _) =
        ContextEconomy::default().record(ContextCompactionTier::StructuralTrim24, 1_000, 800);
    let output =
        OutputSavings::from_provider_request_bytes("compatibility-fixture", 800, 400, 400, true);
    let (mixed, _) = conversation.record_tool_output(output.clone());
    let old: CtxEraEconomy =
        serde_json::from_value(serde_json::to_value(mixed).expect("new economy serializes"))
            .expect("ctx-era economy decoder accepts additive output state");
    assert_eq!(
        old.last_event.map(|event| event.tier),
        Some(ContextCompactionTier::StructuralTrim24)
    );

    let (output_only, _) = ContextEconomy::default().record_tool_output(output);
    let old: CtxEraEconomy = serde_json::from_value(
        serde_json::to_value(output_only).expect("output-only economy serializes"),
    )
    .expect("ctx-era economy decoder ignores output-only child state");
    assert!(old.last_event.is_none());
}

#[test]
fn legacy_context_footprint_without_accounting_remains_decodable() {
    let legacy = serde_json::json!({
        "input_tokens": 8,
        "output_tokens": 2,
        "cached_input_tokens": 3,
        "used_tokens": 13,
        "context_window": 128000,
        "reserved_output_tokens": 4096,
        "soft_threshold_tokens": 108800,
        "estimated_turns_to_threshold": 7,
        "truth": "estimated"
    });
    let decoded: ContextFootprint =
        serde_json::from_value(legacy).expect("legacy footprint remains additive");
    assert_eq!(decoded.truth, ContextFootprintTruth::Estimated);
    assert_eq!(decoded.accounting, None);
}
