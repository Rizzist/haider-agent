#![allow(clippy::expect_used)]

use haider_protocol::task_outcome::TaskOutcomeV1;
use haider_tools::{TASK_OUTCOME_REASON_MAX_BYTES, parse_task_outcome};

#[test]
fn task_outcome_v1_has_strict_bounded_arguments() {
    let valid =
        serde_json::json!({"status":"failure", "reason":"x".repeat(TASK_OUTCOME_REASON_MAX_BYTES)});
    let parsed = parse_task_outcome(valid.clone()).expect("exact byte bound");
    assert_eq!(serde_json::to_value(parsed).expect("round trip"), valid);
    for args in [
        serde_json::json!({"status":"failure", "reason":"é".repeat(TASK_OUTCOME_REASON_MAX_BYTES / 2 + 1)}),
        serde_json::json!({"status":"failure", "reason":"\u{1b}[31mred"}),
        serde_json::json!({"status":"failure", "reason":"\n"}),
        serde_json::json!({"status":"failure", "reason":"   "}),
        serde_json::json!({"status":"success", "reason":"no success bypass"}),
        serde_json::json!({"status":"failure", "reason":"x", "exit_code":0}),
        serde_json::json!({"status":"failure"}),
        serde_json::json!("failure"),
    ] {
        assert!(parse_task_outcome(args.clone()).is_err(), "accepted {args}");
    }
    assert_eq!(
        parse_task_outcome(serde_json::json!({"status":"failure","reason":"Missing input"}))
            .expect("valid"),
        TaskOutcomeV1::Failure {
            reason: "Missing input".into()
        }
    );
}
