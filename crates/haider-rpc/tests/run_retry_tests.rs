use haider_rpc::haider_protocol::ids::{RunId, SessionId};
use haider_rpc::{CommandId, FEATURE_RUN_RETRY_V1, RequestBody, ResponseBody};

/// MUTATION CHECK: rename the feature or wire method. Expected runtime
/// failure: clients cannot feature-gate/send the daemon's `run.retry` seam.
#[test]
fn run_retry_request_is_additive_and_feature_named() {
    assert_eq!(FEATURE_RUN_RETRY_V1, "run_retry_v1");
    let request = RequestBody::RunRetry {
        command_id: CommandId::new("retry-command"),
        session_id: SessionId::new("retry-session"),
        worker_generation: 7,
    };
    let json = serde_json::to_value(&request).expect("serialize request");
    assert_eq!(
        json,
        serde_json::json!({
            "method": "run.retry",
            "command_id": "retry-command",
            "session_id": "retry-session",
            "worker_generation": 7,
        })
    );
    assert_eq!(
        serde_json::from_value::<RequestBody>(json).expect("decode request"),
        request
    );
}

/// MUTATION CHECK: omit either source coordinate or report the failed run as
/// the fresh run. Expected runtime failure: the exact response shape below
/// no longer lets a client bind the new run to the reused user turn.
#[test]
fn run_retry_response_carries_fresh_and_source_coordinates() {
    let response = ResponseBody::RunRetry {
        session_id: SessionId::new("retry-session"),
        run_id: RunId::new("fresh-run"),
        failed_run_id: RunId::new("failed-run"),
        user_seq: 12,
        accepted_seq: 21,
        worker_generation: 7,
    };
    let json = serde_json::to_value(&response).expect("serialize response");
    assert_eq!(
        json,
        serde_json::json!({
            "method": "run.retry",
            "session_id": "retry-session",
            "run_id": "fresh-run",
            "failed_run_id": "failed-run",
            "user_seq": 12,
            "accepted_seq": 21,
            "worker_generation": 7,
        })
    );
    assert_eq!(
        serde_json::from_value::<ResponseBody>(json).expect("decode response"),
        response
    );
}
