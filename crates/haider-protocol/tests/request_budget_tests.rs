#![allow(clippy::expect_used)]

use haider_protocol::headless::RunBudgetV1;
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::request_budget::{
    PROVIDER_REQUEST_BUDGET_EXTENSION_KIND, RequestBudgetContinuationV1, RequestBudgetPhaseV1,
    RequestBudgetStatusV1, RequestBudgetV1,
};

#[test]
fn request_budget_defaults_allow_two_tranches_and_validate_order() {
    let budget = RequestBudgetV1::default();
    assert_eq!((budget.tranche, budget.hard_cap), (32, 64));
    assert!(budget.validate().is_ok());
    for (tranche, hard_cap) in [(0, 64), (32, 0), (65, 64)] {
        assert!(RequestBudgetV1 { tranche, hard_cap }.validate().is_err());
    }
    assert!(
        RequestBudgetV1 {
            tranche: 1,
            hard_cap: 1
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn legacy_run_budget_omits_request_policy_and_new_pin_roundtrips() {
    let legacy: RunBudgetV1 = serde_json::from_str("{}").expect("legacy budget");
    assert!(legacy.is_empty());
    assert_eq!(
        serde_json::to_value(legacy).expect("legacy encodes"),
        serde_json::json!({})
    );
    let pinned: RunBudgetV1 = serde_json::from_value(serde_json::json!({
        "request_budget": {"tranche": 40, "hard_cap": 80}
    }))
    .expect("new budget");
    assert!(!pinned.is_empty());
    assert!(
        !pinned.has_shared_limits(),
        "request-only policy does not start a usage monitor"
    );
    assert_eq!(
        pinned.request_budget,
        Some(RequestBudgetV1 {
            tranche: 40,
            hard_cap: 80
        })
    );
}

#[test]
fn request_budget_extension_retains_typed_coordinates_and_phase_specific_model_note() {
    let mut status = RequestBudgetStatusV1 {
        used: 32,
        budget: RequestBudgetV1::default(),
        phase: RequestBudgetPhaseV1::SoftBound,
        continuation: RequestBudgetContinuationV1 {
            session_id: SessionId::new("budget-session"),
            run_id: RunId::new("budget-run"),
            branch_id: None,
            agent_id: None,
        },
    };
    let item = status.to_extension_item().expect("typed carrier");
    assert_eq!(
        RequestBudgetStatusV1::from_extension_item(&item),
        Some(status.clone())
    );
    let note = status.model_note();
    let payload: serde_json::Value = serde_json::from_str(note.lines().nth(1).expect("JSON line"))
        .expect("model note carries machine-readable JSON");
    assert_eq!(payload["type"], PROVIDER_REQUEST_BUDGET_EXTENSION_KIND);
    assert_eq!(payload["used"], 32);
    assert_eq!(payload["phase"], "soft_bound");
    assert_eq!(payload["continuation"]["run_id"], "budget-run");
    assert!(note.contains("Finish the task or record a checkpoint"));
    status.phase = RequestBudgetPhaseV1::HardBound;
    status.used = 64;
    let hard_note = status.model_note();
    assert!(hard_note.contains("stopped at its hard request cap"));
    assert!(hard_note.contains("later turns have a fresh request budget"));
    assert!(!hard_note.contains("Continue within the remaining"));
}
