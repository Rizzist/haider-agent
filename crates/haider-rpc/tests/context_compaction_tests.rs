#![allow(clippy::expect_used)]

use haider_protocol::ids::{RunId, SessionId};
use haider_rpc::{CommandId, FEATURE_CONTEXT_COMPACTION_V1, RequestBody, ResponseBody};

#[test]
fn session_compact_request_and_response_are_additive_and_golden() {
    assert_eq!(FEATURE_CONTEXT_COMPACTION_V1, "context_compaction_v1");
    let request = RequestBody::SessionCompact {
        command_id: CommandId::new("compact-command"),
        session_id: SessionId::new("session-a"),
        worker_generation: 7,
    };
    assert_eq!(
        serde_json::to_value(&request).expect("request JSON"),
        serde_json::json!({
            "method": "session.compact",
            "command_id": "compact-command",
            "session_id": "session-a",
            "worker_generation": 7
        })
    );
    let response = ResponseBody::SessionCompact {
        session_id: SessionId::new("session-a"),
        run_id: RunId::new("compact-run"),
        accepted_seq: 42,
        worker_generation: 7,
    };
    assert_eq!(
        serde_json::to_value(&response).expect("response JSON"),
        serde_json::json!({
            "method": "session.compact",
            "session_id": "session-a",
            "run_id": "compact-run",
            "accepted_seq": 42,
            "worker_generation": 7
        })
    );
}
