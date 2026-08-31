#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::context::{
    ContextCompactionTier, ContextEconomy, ContextFootprint, ContextFootprintTruth,
    ContextSavingsEvent, ContextSavingsMeasurement,
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
    assert_eq!(structural.removed_tool_call_ids, ["call-a", "call-b"]);

    let carrier = structural.extension_item().expect("encode savings event");
    assert_eq!(
        ContextSavingsEvent::from_extension_item(&carrier),
        Some(structural)
    );
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
