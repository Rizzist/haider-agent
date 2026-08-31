#![allow(clippy::expect_used)]

use super::{RunBudgetDecisionReasonV1, RunBudgetDecisionV1, RunBudgetExhaustedV1};

/// MUTATION CHECK: make the additive decision field required. Stored
/// pre-decision budget events must remain readable after this extension.
#[test]
fn budget_exhaustion_decodes_without_decision_detail() {
    let event: RunBudgetExhaustedV1 = serde_json::from_value(serde_json::json!({
        "dimension": "cost",
        "limit": 10,
        "usage": {
            "logical_input_tokens": 1,
            "billed_output_tokens": 2,
            "additional_reasoning_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "total_tokens": 3,
            "elapsed_ms": 4
        }
    }))
    .expect("legacy budget exhaustion decodes");

    assert!(event.decision.is_none());
}

/// MUTATION CHECK: serialize an unavailable price as zero or omit the
/// provider/model identity. A cap must never conceal an estimate gap.
#[test]
fn budget_decision_preserves_unknown_pricing_without_guessing() {
    let decision = RunBudgetDecisionV1 {
        spent: 7,
        projected: None,
        cap: 10,
        reason: RunBudgetDecisionReasonV1::PricingUnavailable {
            provider: "fake".into(),
            model: "unpriced-model".into(),
        },
    };

    assert_eq!(
        serde_json::to_value(&decision).expect("decision serializes"),
        serde_json::json!({
            "spent": 7,
            "projected": null,
            "cap": 10,
            "reason": {
                "type": "pricing_unavailable",
                "provider": "fake",
                "model": "unpriced-model"
            }
        })
    );

    let exhausted = RunBudgetExhaustedV1 {
        dimension: super::RunBudgetDimensionV1::Cost,
        limit: 10,
        usage: super::HeadlessRunUsageV1::default(),
        decision: Some(decision),
    };
    let summary = exhausted.summary();
    assert!(summary.contains("spent 7"));
    assert!(summary.contains("projected unavailable"));
    assert!(summary.contains("cap 10"));
    assert!(summary.contains("fake"));
    assert!(summary.contains("unpriced-model"));
}
