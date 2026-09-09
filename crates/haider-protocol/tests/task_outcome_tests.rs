#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::error::ErrorCode;
use haider_protocol::state::RunState;
use haider_protocol::task_outcome::TaskOutcomeV1;

#[test]
fn task_outcome_terminal_metadata_is_additive_to_the_existing_state_carrier() {
    let outcome = TaskOutcomeV1::Failure {
        reason: "Missing input".into(),
    };
    let mut payload =
        serde_json::to_value(EventPayload::RunState(RunState::Errored)).expect("legacy carrier");
    let legacy = payload.clone();
    payload["task_outcome_version"] = serde_json::json!(1);
    payload["task_outcome"] = serde_json::to_value(&outcome).expect("outcome");
    assert_eq!(
        payload["task_outcome"],
        serde_json::json!({"status":"failure", "reason":"Missing input"})
    );
    for value in [legacy, payload.clone()] {
        assert_eq!(
            serde_json::from_value::<EventPayload>(value).expect("old/new carrier"),
            EventPayload::RunState(RunState::Errored)
        );
    }
    assert_eq!(
        serde_json::from_value::<TaskOutcomeV1>(payload["task_outcome"].clone())
            .expect("typed metadata"),
        outcome
    );
    assert_eq!(
        serde_json::to_value(ErrorCode::TaskFailed).expect("error code"),
        "task_failed"
    );
    assert_eq!(ErrorCode::TaskFailed.as_str(), "task_failed");
    assert_eq!(ErrorCode::TaskFailed.as_subcode(), "task-failed");
}
