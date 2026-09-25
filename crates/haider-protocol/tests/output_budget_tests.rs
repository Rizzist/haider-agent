//! Session output-budget source classification and application (973).

use haider_protocol::output_budget::{
    DEFAULT_OUTPUT_LIMIT, LEGACY_CLIENT_DEFAULT_OUTPUT_LIMITS, OutputBudgetClampV1,
    SessionOutputBudgetSourceV1,
};

#[test]
fn legacy_budgets_classify_client_defaults_as_derived() {
    for value in LEGACY_CLIENT_DEFAULT_OUTPUT_LIMITS {
        assert_eq!(
            SessionOutputBudgetSourceV1::classify(None, value),
            SessionOutputBudgetSourceV1::Derived
        );
    }
    assert_eq!(
        SessionOutputBudgetSourceV1::classify(None, 12_000),
        SessionOutputBudgetSourceV1::UserSet { requested: 12_000 }
    );
    assert_eq!(
        SessionOutputBudgetSourceV1::classify(Some(SessionOutputBudgetSourceV1::Derived), 12_000),
        SessionOutputBudgetSourceV1::Derived
    );
}

#[test]
fn derived_budgets_follow_the_model_and_user_budgets_clamp_with_notice() {
    let derived = SessionOutputBudgetSourceV1::Derived;
    assert_eq!(
        derived.apply(128_000, 128_000).max_tokens,
        DEFAULT_OUTPUT_LIMIT
    );
    assert_eq!(derived.apply(16_384, 16_384).max_tokens, 16_384);
    assert_eq!(derived.apply(16_384, 16_384).clamped, None);

    let user = SessionOutputBudgetSourceV1::UserSet { requested: 30_000 };
    let clamped = user.apply(16_384, 16_384);
    assert_eq!(clamped.max_tokens, 16_384);
    assert_eq!(
        clamped.clamped,
        Some(OutputBudgetClampV1 {
            requested: 30_000,
            max_output_tokens: 16_384
        })
    );
    assert_eq!(user.apply(128_000, 128_000).max_tokens, 30_000);
    assert_eq!(user.apply(128_000, 128_000).clamped, None);
}

/// S1: an unsourced 8,192 guess bounds only the derived budget; a user-set
/// budget is bounded by the row's larger explicit ceiling instead.
#[test]
fn user_budgets_use_the_explicit_ceiling_when_the_model_maximum_is_a_guess() {
    let derived = SessionOutputBudgetSourceV1::Derived;
    assert_eq!(derived.apply(8_192, 384_000).max_tokens, 8_192);
    assert_eq!(derived.apply(8_192, 384_000).clamped, None);

    let user = SessionOutputBudgetSourceV1::UserSet { requested: 30_000 };
    let budget = user.apply(8_192, 384_000);
    assert_eq!(budget.max_tokens, 30_000);
    assert_eq!(budget.clamped, None);

    let over = SessionOutputBudgetSourceV1::UserSet { requested: 500_000 }.apply(8_192, 384_000);
    assert_eq!(over.max_tokens, 384_000);
    assert_eq!(
        over.clamped,
        Some(OutputBudgetClampV1 {
            requested: 500_000,
            max_output_tokens: 384_000
        })
    );
}

#[test]
fn source_wire_shape_is_tagged() -> Result<(), serde_json::Error> {
    assert_eq!(
        serde_json::to_value(SessionOutputBudgetSourceV1::UserSet { requested: 7 })?,
        serde_json::json!({"kind": "user_set", "requested": 7})
    );
    assert_eq!(
        serde_json::to_value(SessionOutputBudgetSourceV1::Derived)?,
        serde_json::json!({"kind": "derived"})
    );
    Ok(())
}
