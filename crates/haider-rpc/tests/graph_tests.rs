#![allow(clippy::expect_used)]

use haider_protocol::graph::{
    GraphEvidenceTally, GraphInspectSnapshot, GraphNodeStatus, GraphPhase, GraphStatus,
};
use haider_protocol::ids::{GraphId, GraphRunSetId, ItemId, MenuId, SessionId};
use haider_rpc::{
    CommandId, ErrorData, FEATURE_CONVERGENCE_GRAPH_V1, FEATURE_CONVERGENCE_GRAPH_V2,
    FEATURE_CONVERGENCE_GRAPH_V3, FEATURE_CONVERGENCE_GRAPH_V4, FEATURE_LOOM_PIPE_DAG_V1,
    FEATURE_WORKFLOW_CATALOG_V1, FEATURE_WORKFLOW_INSTANCE_V1, RequestBody, ResponseBody,
    TodoGraphOpenedWire, WorkflowCatalogEntryV1, WorkflowInstanceSourceV1, WorkflowInstanceV1,
};

#[test]
fn graph_request_and_response_family_has_exact_additive_wire_shapes() {
    assert_eq!(FEATURE_CONVERGENCE_GRAPH_V1, "convergence_graph_v1");
    assert_eq!(FEATURE_CONVERGENCE_GRAPH_V2, "convergence_graph_v2");
    assert_eq!(FEATURE_CONVERGENCE_GRAPH_V3, "convergence_graph_v3");
    assert_eq!(FEATURE_CONVERGENCE_GRAPH_V4, "convergence_graph_v4");
    assert_eq!(FEATURE_WORKFLOW_CATALOG_V1, "workflow_catalog_v1");
    assert_eq!(FEATURE_LOOM_PIPE_DAG_V1, "loom_pipe_dag_v1");
    assert_eq!(FEATURE_WORKFLOW_INSTANCE_V1, "workflow_instance_v1");
    let session_id = SessionId::new("session-graph");
    let graph_id = GraphId::new("graph-1");
    let builtin_template =
        haider_protocol::graph::graph_template(haider_protocol::graph::SHIP_LOOP_TEMPLATE)
            .expect("built-in template");
    let builtin_digest = haider_protocol::graph::graph_template_digest(&builtin_template);
    let cases = vec![
        (
            serde_json::to_value(RequestBody::GraphPin {
                command_id: CommandId::new("pin-command"),
                session_id: session_id.clone(),
                worker_generation: 7,
                template: "ship-loop".into(),
                expected_digest: None,
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
            serde_json::to_value(RequestBody::GraphPin {
                command_id: CommandId::new("pin-command-fenced"),
                session_id: session_id.clone(),
                worker_generation: 7,
                template: "ship-loop".into(),
                expected_digest: Some("blake3:observed".into()),
            })
            .expect("fenced pin request"),
            serde_json::json!({
                "method": "graph.pin",
                "command_id": "pin-command-fenced",
                "session_id": "session-graph",
                "worker_generation": 7,
                "template": "ship-loop",
                "expected_digest": "blake3:observed"
            }),
        ),
        (
            serde_json::to_value(RequestBody::GraphRunSetOpen {
                command_id: CommandId::new("run-set-command"),
                session_id: session_id.clone(),
                worker_generation: 7,
                plan_item_id: ItemId::new("plan-item"),
                plan_event_seq: 12,
            })
            .expect("run-set request"),
            serde_json::json!({
                "method": "graph.run_set.open",
                "command_id": "run-set-command",
                "session_id": "session-graph",
                "worker_generation": 7,
                "plan_item_id": "plan-item",
                "plan_event_seq": 12
            }),
        ),
        (
            serde_json::to_value(RequestBody::GraphSwitch {
                command_id: CommandId::new("switch-command-fenced"),
                session_id: session_id.clone(),
                worker_generation: 7,
                old_graph_id: graph_id.clone(),
                template: "staggered".into(),
                expected_digest: Some("blake3:observed-replacement".into()),
            })
            .expect("fenced switch request"),
            serde_json::json!({
                "method": "graph.switch",
                "command_id": "switch-command-fenced",
                "session_id": "session-graph",
                "worker_generation": 7,
                "old_graph_id": "graph-1",
                "template": "staggered",
                "expected_digest": "blake3:observed-replacement"
            }),
        ),
        (
            serde_json::to_value(RequestBody::WorkflowInstance {
                workflow_id: "ship-loop".into(),
                template_digest: Some(builtin_digest.clone()),
            })
            .expect("workflow instance request"),
            serde_json::json!({
                "method": "workflow.instance",
                "workflow_id": "ship-loop",
                "template_digest": builtin_digest
            }),
        ),
        (
            serde_json::to_value(ResponseBody::WorkflowInstance {
                instance: Some(WorkflowInstanceV1 {
                    id: builtin_template.name.clone(),
                    revision: builtin_template.version,
                    digest: None,
                    template_digest: haider_protocol::graph::graph_template_digest(
                        &builtin_template,
                    ),
                    pipe_version: None,
                    source: WorkflowInstanceSourceV1::BuiltIn,
                    node_metadata: None,
                    compiled_template: builtin_template.clone(),
                }),
            })
            .expect("workflow instance response"),
            serde_json::json!({
                "method": "workflow.instance",
                "instance": {
                    "id": builtin_template.name,
                    "revision": builtin_template.version,
                    "template_digest": haider_protocol::graph::graph_template_digest(&builtin_template),
                    "source": "built_in",
                    "compiled_template": builtin_template
                }
            }),
        ),
        (
            serde_json::to_value(RequestBody::GraphSwitch {
                command_id: CommandId::new("switch-command"),
                session_id: session_id.clone(),
                worker_generation: 7,
                old_graph_id: graph_id.clone(),
                template: "staggered".into(),
                expected_digest: None,
            })
            .expect("switch request"),
            serde_json::json!({
                "method": "graph.switch",
                "command_id": "switch-command",
                "session_id": "session-graph",
                "worker_generation": 7,
                "old_graph_id": "graph-1",
                "template": "staggered"
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
            serde_json::to_value(RequestBody::GraphInspect {
                session_id: session_id.clone(),
                cursor: Some("opaque-cursor".into()),
                limit: 25,
            })
            .expect("inspect request"),
            serde_json::json!({
                "method": "graph.inspect",
                "session_id": "session-graph",
                "cursor": "opaque-cursor",
                "limit": 25
            }),
        ),
        (
            serde_json::to_value(ResponseBody::GraphInspect {
                snapshot: GraphInspectSnapshot {
                    through_seq: 42,
                    status: None,
                    runs: Vec::new(),
                    template_rollups: Vec::new(),
                    tool_selection: Vec::new(),
                    evidence: Vec::new(),
                },
                next_cursor: Some("next-cursor".into()),
            })
            .expect("inspect response"),
            serde_json::json!({
                "method": "graph.inspect",
                "snapshot": {
                    "through_seq": 42,
                    "runs": [],
                    "template_rollups": [],
                    "tool_selection": [],
                    "evidence": []
                },
                "next_cursor": "next-cursor"
            }),
        ),
        (
            serde_json::to_value(ResponseBody::GraphRunSetOpen {
                session_id: session_id.clone(),
                run_set_id: GraphRunSetId::new("run-set-1"),
                root_graph_id: graph_id.clone(),
                plan_item_id: ItemId::new("plan-item"),
                plan_event_seq: 12,
                template: "ship-loop".into(),
                digest: "blake3:template".into(),
                run_set_opened_seq: 13,
                through_seq: 16,
                children: vec![TodoGraphOpenedWire {
                    todo_id: 10,
                    depends_on_todo_id: None,
                    child_graph_id: GraphId::new("todo-child-10"),
                    attached_seq: 14,
                    pinned_seq: 15,
                    opened_seq: Some(16),
                }],
                worker_generation: 7,
            })
            .expect("run-set response"),
            serde_json::json!({
                "method": "graph.run_set.open",
                "session_id": "session-graph",
                "run_set_id": "run-set-1",
                "root_graph_id": "graph-1",
                "plan_item_id": "plan-item",
                "plan_event_seq": 12,
                "template": "ship-loop",
                "digest": "blake3:template",
                "run_set_opened_seq": 13,
                "through_seq": 16,
                "children": [{
                    "todo_id": 10,
                    "child_graph_id": "todo-child-10",
                    "attached_seq": 14,
                    "pinned_seq": 15,
                    "opened_seq": 16
                }],
                "worker_generation": 7
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
            serde_json::to_value(ResponseBody::GraphSwitch {
                session_id: session_id.clone(),
                old_graph_id: graph_id.clone(),
                new_graph_id: GraphId::new("graph-2"),
                template: "staggered".into(),
                digest: "blake3:replacement".into(),
                superseded_seq: 20,
                pinned_seq: 21,
                opened_seq: 22,
                worker_generation: 7,
            })
            .expect("switch response"),
            serde_json::json!({
                "method": "graph.switch",
                "session_id": "session-graph",
                "old_graph_id": "graph-1",
                "new_graph_id": "graph-2",
                "template": "staggered",
                "digest": "blake3:replacement",
                "superseded_seq": 20,
                "pinned_seq": 21,
                "opened_seq": 22,
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
                    template_version: 0,
                    start_node: None,
                    phase: GraphPhase::Active,
                    current_node: Some(haider_protocol::graph::verify_node()),
                    ready_nodes: Vec::new(),
                    attempt: 2,
                    nodes: vec![GraphNodeStatus {
                        node: haider_protocol::graph::verify_node(),
                        gate: None,
                        executor: None,
                        attempts_opened: 2,
                        current_attempt: Some(2),
                        evidence: GraphEvidenceTally {
                            green: 2,
                            red: 1,
                            effective_green: 1,
                            standing_red: 0,
                        },
                        evidence_slots: Vec::new(),
                        satisfied: false,
                    }],
                    blocked_reason: None,
                    pending_menu: Some(MenuId::new("ship-confirm-2")),
                    pending_menus: Vec::new(),
                    run_set: None,
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
        template_version: 0,
        start_node: None,
        phase,
        current_node: Some(haider_protocol::graph::verify_node()),
        ready_nodes: Vec::new(),
        attempt: 8,
        nodes: Vec::new(),
        blocked_reason,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
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
    assert!(matches!(
        pin,
        RequestBody::GraphPin {
            expected_digest: None,
            ..
        }
    ));
}

#[test]
fn workflow_revision_conflict_is_typed_and_carries_current_authority() {
    let body = ResponseBody::Error {
        code: haider_rpc::ERROR_CODE_REVISION_CONFLICT.into(),
        message: "workflow changed before selection".into(),
        retryable: false,
        data: Some(ErrorData::WorkflowRevisionConflict {
            expected_digest: "old-template-digest".into(),
            current_digest: "current-template-digest".into(),
            current_revision: 9,
        }),
    };
    assert_eq!(
        serde_json::to_value(body).expect("conflict encodes"),
        serde_json::json!({
            "method": "error",
            "code": "revision_conflict",
            "message": "workflow changed before selection",
            "retryable": false,
            "data": {
                "kind": "workflow_revision_conflict",
                "expected_digest": "old-template-digest",
                "current_digest": "current-template-digest",
                "current_revision": 9
            }
        })
    );
}

/// MUTATION CHECK: remove the catalog field's serde default/skip-empty,
/// flatten or retype either nested authority record, or make an old reader
/// reject the additive field. Expected runtime failure: one of the three
/// compatibility directions below changes.
#[test]
fn workflow_catalog_is_additive_verbatim_and_unknown_origin_tolerant() {
    let template =
        haider_protocol::graph::graph_template(haider_protocol::graph::SHIP_LOOP_TEMPLATE)
            .expect("built-in template");
    let new = ResponseBody::LoomList {
        agent_types: Vec::new(),
        workflows: Vec::new(),
        cli_present: std::collections::BTreeMap::new(),
        workflow_catalog: vec![WorkflowCatalogEntryV1::BuiltIn {
            id: template.name.clone(),
            main_session_eligible: true,
            template: template.clone(),
        }],
        archived_entries: Vec::new(),
    };
    let new_value = serde_json::to_value(&new).expect("new loom.list encodes");
    assert_eq!(new_value["workflow_catalog"][0]["origin"], "built_in");
    assert_eq!(new_value["workflow_catalog"][0]["id"], template.name);
    assert_eq!(
        new_value["workflow_catalog"][0]["main_session_eligible"],
        true
    );
    assert_eq!(
        new_value["workflow_catalog"][0]["template"],
        serde_json::to_value(template).expect("authoritative template encodes")
    );

    #[derive(serde::Deserialize)]
    #[serde(tag = "method")]
    enum PreCatalogResponse {
        #[serde(rename = "loom.list")]
        LoomList {
            #[serde(default)]
            workflows: Vec<serde_json::Value>,
        },
        #[serde(other)]
        Unknown,
    }
    let PreCatalogResponse::LoomList { workflows } =
        serde_json::from_value(new_value).expect("old client ignores additive catalog")
    else {
        panic!("old client must retain the loom.list method");
    };
    assert!(workflows.is_empty());

    let old: ResponseBody = serde_json::from_value(serde_json::json!({"method": "loom.list"}))
        .expect("new client decodes old loom.list");
    let ResponseBody::LoomList {
        workflow_catalog, ..
    } = old
    else {
        panic!("old loom.list keeps its method");
    };
    assert!(workflow_catalog.is_empty());
    assert_eq!(
        serde_json::to_value(ResponseBody::LoomList {
            agent_types: Vec::new(),
            workflows: Vec::new(),
            cli_present: std::collections::BTreeMap::new(),
            workflow_catalog,
            archived_entries: Vec::new(),
        })
        .expect("empty catalog re-encodes"),
        serde_json::json!({"method": "loom.list"})
    );

    let future: ResponseBody = serde_json::from_value(serde_json::json!({
        "method": "loom.list",
        "workflow_catalog": [{
            "origin": "remote_marketplace",
            "id": "future",
            "main_session_eligible": true,
            "future": {"ignored": true}
        }]
    }))
    .expect("future catalog origin is tolerated");
    assert!(matches!(
        future,
        ResponseBody::LoomList {
            workflow_catalog,
            ..
        } if workflow_catalog == vec![WorkflowCatalogEntryV1::Unknown]
    ));
}
