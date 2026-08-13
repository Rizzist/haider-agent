#![allow(clippy::expect_used)]

use haider_protocol::graph::{
    GraphEvidenceTally, GraphNodeName, GraphNodeStatus, GraphPhase, GraphStatus,
};
use haider_protocol::ids::{GraphId, MenuId, SessionId};
use haider_rpc::{CommandId, FEATURE_CONVERGENCE_GRAPH_V1, RequestBody, ResponseBody};

#[test]
fn graph_request_and_response_family_has_exact_additive_wire_shapes() {
    assert_eq!(FEATURE_CONVERGENCE_GRAPH_V1, "convergence_graph_v1");
    let session_id = SessionId::new("session-graph");
    let graph_id = GraphId::new("graph-1");
    let cases = vec![
        (
            serde_json::to_value(RequestBody::GraphPin {
                command_id: CommandId::new("pin-command"),
                session_id: session_id.clone(),
                worker_generation: 7,
                template: "ship-loop".into(),
            })
            .expect("pin request"),
            serde_json::json!({
                "method": "graph.pin",
                "command_id": "pin-command",
                "session_id": "session-graph",
                "worker_generation": 7,
                "template": "ship-loop"
            }),
        ),
        (
            serde_json::to_value(RequestBody::GraphAbandon {
                command_id: CommandId::new("abandon-command"),
                session_id: session_id.clone(),
                worker_generation: 7,
                why: "release deferred".into(),
            })
            .expect("abandon request"),
            serde_json::json!({
                "method": "graph.abandon",
                "command_id": "abandon-command",
                "session_id": "session-graph",
                "worker_generation": 7,
                "why": "release deferred"
            }),
        ),
        (
            serde_json::to_value(RequestBody::GraphStatus {
                session_id: session_id.clone(),
            })
            .expect("status request"),
            serde_json::json!({
                "method": "graph.status",
                "session_id": "session-graph"
            }),
        ),
        (
            serde_json::to_value(ResponseBody::GraphPin {
                session_id: session_id.clone(),
                graph_id: graph_id.clone(),
                template: "ship-loop".into(),
                digest: "blake3:template".into(),
                pinned_seq: 10,
                opened_seq: 11,
                worker_generation: 7,
            })
            .expect("pin response"),
            serde_json::json!({
                "method": "graph.pin",
                "session_id": "session-graph",
                "graph_id": "graph-1",
                "template": "ship-loop",
                "digest": "blake3:template",
                "pinned_seq": 10,
                "opened_seq": 11,
                "worker_generation": 7
            }),
        ),
        (
            serde_json::to_value(ResponseBody::GraphAbandon {
                session_id: session_id.clone(),
                graph_id: graph_id.clone(),
                abandoned_seq: 42,
                worker_generation: 7,
            })
            .expect("abandon response"),
            serde_json::json!({
                "method": "graph.abandon",
                "session_id": "session-graph",
                "graph_id": "graph-1",
                "abandoned_seq": 42,
                "worker_generation": 7
            }),
        ),
        (
            serde_json::to_value(ResponseBody::GraphStatus {
                status: Some(GraphStatus {
                    graph_id,
                    template: "ship-loop".into(),
                    digest: "blake3:template".into(),
                    phase: GraphPhase::Active,
                    current_node: Some(GraphNodeName::Verify),
                    attempt: 2,
                    nodes: vec![GraphNodeStatus {
                        node: GraphNodeName::Verify,
                        attempts_opened: 2,
                        current_attempt: Some(2),
                        evidence: GraphEvidenceTally {
                            green: 2,
                            red: 1,
                            effective_green: 1,
                            standing_red: 0,
                        },
                        satisfied: false,
                    }],
                    blocked_reason: None,
                    pending_menu: Some(MenuId::new("ship-confirm-2")),
                }),
            })
            .expect("status response"),
            serde_json::json!({
                "method": "graph.status",
                "status": {
                    "graph_id": "graph-1",
                    "template": "ship-loop",
                    "digest": "blake3:template",
                    "phase": "active",
                    "current_node": "VERIFY",
                    "attempt": 2,
                    "nodes": [{
                        "node": "VERIFY",
                        "attempts_opened": 2,
                        "current_attempt": 2,
                        "evidence": {
                            "green": 2,
                            "red": 1,
                            "effective_green": 1,
                            "standing_red": 0
                        },
                        "satisfied": false
                    }],
                    "pending_menu": "ship-confirm-2"
                }
            }),
        ),
    ];
    for (actual, expected) in cases {
        assert_eq!(actual, expected);
    }
}

#[test]
fn graph_status_terminal_variants_have_stable_typed_shapes() {
    let base = |phase, blocked_reason| GraphStatus {
        graph_id: GraphId::new("graph-terminal"),
        template: "ship-loop".into(),
        digest: "template-digest".into(),
        phase,
        current_node: Some(GraphNodeName::Verify),
        attempt: 8,
        nodes: Vec::new(),
        blocked_reason,
        pending_menu: None,
    };
    let blocked = serde_json::to_value(ResponseBody::GraphStatus {
        status: Some(base(
            GraphPhase::Blocked,
            Some(haider_protocol::graph::GraphBlockReason::RoundsExhausted),
        )),
    })
    .expect("blocked status");
    assert_eq!(blocked["status"]["phase"], "blocked");
    assert_eq!(blocked["status"]["blocked_reason"], "rounds-exhausted");

    let completed = serde_json::to_value(ResponseBody::GraphStatus {
        status: Some(base(GraphPhase::Completed, None)),
    })
    .expect("completed status");
    assert_eq!(completed["status"]["phase"], "completed");
    assert!(completed["status"].get("blocked_reason").is_none());
    assert!(completed["status"].get("pending_menu").is_none());
}

#[test]
fn old_method_decoders_and_new_graph_decoders_remain_tolerant() {
    let unknown: RequestBody = serde_json::from_value(serde_json::json!({
        "method": "graph.attach_children_v2",
        "future": true
    }))
    .expect("unknown future method tolerates");
    assert_eq!(unknown, RequestBody::Unknown);

    let pin: RequestBody = serde_json::from_value(serde_json::json!({
        "method": "graph.pin",
        "command_id": "pin-command",
        "session_id": "session-graph",
        "worker_generation": 7,
        "template": "ship-loop",
        "future_field": {"ignored": true}
    }))
    .expect("new graph decoder ignores future object fields");
    assert!(matches!(pin, RequestBody::GraphPin { .. }));
}
