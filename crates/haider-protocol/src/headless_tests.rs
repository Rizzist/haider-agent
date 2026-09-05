#![allow(clippy::expect_used)]

use super::{
    RunBudgetDecisionReasonV1, RunBudgetDecisionV1, RunBudgetExhaustedV1, durable_run_terminal_v1,
};
use crate::error::ErrorCode;
use crate::state::RunState;

#[test]
fn legacy_headless_spec_omits_direct_spawn_and_new_pin_round_trips() {
    let legacy = serde_json::json!({"provider":"fake","model":"fake-model","max_output_tokens":64,"fast":false});
    let mut spec: super::HeadlessRunSpecV1 =
        serde_json::from_value(legacy.clone()).expect("legacy spec");
    assert!(spec.agent_spawn.is_none());
    assert_eq!(serde_json::to_value(&spec).expect("legacy encode"), legacy);
    spec.agent_spawn = Some(super::AgentSpawnSpecV1 {
        task: "task".into(),
        prompt: "prompt".into(),
        model: None,
        provider: None,
        agent_type: None,
        workflow: Some("deeper".into()),
        workflow_trigger: Some("dependent_phases".into()),
    });
    let encoded = serde_json::to_value(&spec).expect("new encode");
    assert_eq!(encoded["agent_spawn"]["workflow"], "deeper");
    assert_eq!(
        serde_json::from_value::<super::HeadlessRunSpecV1>(encoded).expect("new decode"),
        spec
    );
}

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

/// MUTATION CHECK: classify a cancellation triggered by durable blocking input
/// as ordinary cancellation, or let a later generic failure replace the first
/// blocking cause. The retained live terminal would change shape.
#[test]
fn durable_terminal_classifier_preserves_blocked_cancellation_and_cause_precedence() {
    assert!(durable_run_terminal_v1(RunState::Thinking, None, false, false, None).is_none());
    assert_eq!(
        durable_run_terminal_v1(
            RunState::Cancelled,
            None,
            false,
            false,
            Some("input_required")
        ),
        Some(super::DurableRunTerminalV1 {
            terminal_kind: "failure",
            error_code: Some("input_required"),
        })
    );
    assert_eq!(
        durable_run_terminal_v1(
            RunState::Errored,
            Some(ErrorCode::Internal),
            false,
            false,
            Some("effect_outcome_unknown")
        ),
        Some(super::DurableRunTerminalV1 {
            terminal_kind: "failure",
            error_code: Some("effect_outcome_unknown"),
        })
    );
    assert_eq!(
        durable_run_terminal_v1(RunState::Cancelled, None, false, true, None),
        Some(super::DurableRunTerminalV1 {
            terminal_kind: "timeout",
            error_code: Some("timeout"),
        })
    );
}
