//! v0.0.963 L3 adapter laws over L2's exact RPC graph shapes.

#![allow(clippy::expect_used)]

use haider_client::{WorkflowGraphProjection, WorkflowGraphRpcAdapter, WorkflowNodeState};
use haider_protocol::graph::{
    GraphNodeName, WorkflowActivationAst, WorkflowActivationCause, WorkflowActivationEdge,
    WorkflowActivationNode, WorkflowEdgeKind, WorkflowGraphJournalEvent, WorkflowGraphStarted,
    WorkflowGraphState, WorkflowGraphWatchEvent, WorkflowGraphWatchPage, WorkflowJoinSemantics,
    WorkflowNodeActivated, WorkflowNodeCompleted, WorkflowNodeInput, WorkflowNodeRejectCode,
    WorkflowNodeRejected, workflow_activation_ast_digest, workflow_evidence_ledger_digest,
    workflow_input_ledger_digest,
};
use haider_protocol::ids::{ArtifactRef, GraphId};
use haider_protocol::pipe::InstructEvidenceRef;

fn artifact(byte: char) -> ArtifactRef {
    ArtifactRef::new(format!("blake3:{}", byte.to_string().repeat(64)))
}

fn evidence(byte: char, evidence_type: &str, parents: Vec<ArtifactRef>) -> InstructEvidenceRef {
    InstructEvidenceRef::new(artifact(byte), evidence_type, 7, parents)
}

fn graph() -> (WorkflowGraphState, WorkflowNodeInput, InstructEvidenceRef) {
    let plan = GraphNodeName::new("PLAN").expect("valid node");
    let verify = GraphNodeName::new("VERIFY").expect("valid node");
    let ast = WorkflowActivationAst {
        workflow_id: "release".to_owned(),
        workflow_digest: "blake3:workflow".to_owned(),
        input_type: "brief".to_owned(),
        output_type: "report".to_owned(),
        nodes: vec![
            WorkflowActivationNode {
                node: plan.clone(),
                input_type: "brief".to_owned(),
                output_type: "report".to_owned(),
                join: WorkflowJoinSemantics {
                    initial_all: vec![1],
                    reactivate_any: vec![3],
                },
                convergence_gate: false,
            },
            WorkflowActivationNode {
                node: verify.clone(),
                input_type: "report".to_owned(),
                output_type: "report".to_owned(),
                join: WorkflowJoinSemantics {
                    initial_all: vec![2],
                    reactivate_any: Vec::new(),
                },
                convergence_gate: false,
            },
        ],
        edges: vec![
            WorkflowActivationEdge {
                id: 1,
                kind: WorkflowEdgeKind::GraphInput,
                from: None,
                to: plan,
                evidence_type: "brief".to_owned(),
            },
            WorkflowActivationEdge {
                id: 2,
                kind: WorkflowEdgeKind::Forward,
                from: Some(GraphNodeName::new("PLAN").expect("valid node")),
                to: verify.clone(),
                evidence_type: "report".to_owned(),
            },
            WorkflowActivationEdge {
                id: 3,
                kind: WorkflowEdgeKind::Back,
                from: Some(verify),
                to: GraphNodeName::new("PLAN").expect("valid node"),
                evidence_type: "brief".to_owned(),
            },
        ],
        max_back_edge_activations: 3,
    };
    let seed = evidence('a', "brief", Vec::new());
    let graph_id = GraphId::new("graph-release");
    let state = WorkflowGraphState::from_started(
        10,
        WorkflowGraphStarted {
            graph_id,
            ast_digest: workflow_activation_ast_digest(&ast),
            ast,
            seed: Some(seed.clone()),
        },
    )
    .expect("valid started graph");
    let input = WorkflowNodeInput {
        edge_id: 1,
        evidence: seed.clone(),
    };
    let output = evidence('b', "report", vec![seed.artifact]);
    (state, input, output)
}

fn activated(state: &WorkflowGraphState, input: WorkflowNodeInput) -> WorkflowGraphJournalEvent {
    WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
        graph_id: state.graph_id.clone(),
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 1,
        activation_order: 1,
        cause: WorkflowActivationCause::ForwardJoin,
        input_ledger_digest: workflow_input_ledger_digest(std::slice::from_ref(&input)),
        inputs: vec![input],
    })
}

fn completed(state: &WorkflowGraphState, output: InstructEvidenceRef) -> WorkflowGraphJournalEvent {
    WorkflowGraphJournalEvent::WorkflowNodeCompleted(WorkflowNodeCompleted {
        graph_id: state.graph_id.clone(),
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 1,
        output_ledger_digest: workflow_evidence_ledger_digest(std::slice::from_ref(&output)),
        outputs: vec![output],
        convergence: None,
    })
}

fn watch(after: u64, through: u64, events: Vec<WorkflowGraphWatchEvent>) -> WorkflowGraphWatchPage {
    WorkflowGraphWatchPage {
        requested_after_cursor: after,
        replay_through_cursor: through,
        next_cursor: events.last().map_or(through, |event| event.cursor),
        events,
    }
}

#[test]
fn l2_watch_events_reduce_to_ready_active_complete_and_input_lights() {
    let (state, input, output) = graph();
    let active = activated(&state, input);
    let complete = completed(&state, output);
    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(adapter.replace(state).expect("valid L2 baseline"))
        .expect("valid baseline");

    assert_eq!(
        projection.node("PLAN").map(|node| node.status),
        Some(WorkflowNodeState::Ready)
    );
    assert_eq!(
        projection
            .node("PLAN")
            .map(|node| node.inputs_present.as_slice()),
        Some([true, false].as_slice())
    );
    let page = adapter
        .apply_page(
            10,
            watch(
                10,
                12,
                vec![
                    WorkflowGraphWatchEvent {
                        cursor: 11,
                        event: active,
                    },
                    WorkflowGraphWatchEvent {
                        cursor: 12,
                        event: complete,
                    },
                ],
            ),
        )
        .expect("valid L2 watch page");
    projection.apply_page(page).expect("valid view page");

    assert_eq!(projection.cursor(), Some(12));
    assert_eq!(
        projection.node("PLAN").map(|node| node.status),
        Some(WorkflowNodeState::Complete)
    );
    let verify = projection.node("VERIFY").expect("verify node");
    assert_eq!(verify.status, WorkflowNodeState::Ready);
    assert_eq!(verify.inputs_present, vec![true]);
}

#[test]
fn seedless_graph_stays_dark_until_its_external_input_arrives() {
    let (seeded, input, _) = graph();
    let seedless = WorkflowGraphState::from_started(
        10,
        WorkflowGraphStarted {
            graph_id: GraphId::new("graph-waiting-for-input"),
            ast_digest: workflow_activation_ast_digest(&seeded.ast),
            ast: seeded.ast,
            seed: None,
        },
    )
    .expect("valid seedless graph");
    let activation = activated(&seedless, input);
    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(adapter.replace(seedless).expect("valid seedless baseline"))
        .expect("valid waiting projection");

    let waiting = projection.node("PLAN").expect("waiting root");
    assert_eq!(waiting.status, WorkflowNodeState::Waiting);
    assert_eq!(waiting.inputs_present, vec![false, false]);

    let page = adapter
        .apply_page(
            10,
            watch(
                10,
                11,
                vec![WorkflowGraphWatchEvent {
                    cursor: 11,
                    event: activation,
                }],
            ),
        )
        .expect("external input activation");
    projection.apply_page(page).expect("lit root projection");

    let active = projection.node("PLAN").expect("active root");
    assert_eq!(active.status, WorkflowNodeState::Active);
    assert_eq!(active.inputs_present, vec![true, false]);
}

#[test]
fn split_cursor_reconnect_matches_one_uninterrupted_l2_page() {
    let (state, input, output) = graph();
    let active = activated(&state, input);
    let complete = completed(&state, output);

    let mut uninterrupted_adapter = WorkflowGraphRpcAdapter::default();
    let mut uninterrupted = WorkflowGraphProjection::default();
    uninterrupted
        .replace(
            uninterrupted_adapter
                .replace(state.clone())
                .expect("valid L2 baseline"),
        )
        .expect("valid baseline");
    let page = uninterrupted_adapter
        .apply_page(
            10,
            watch(
                10,
                12,
                vec![
                    WorkflowGraphWatchEvent {
                        cursor: 11,
                        event: active.clone(),
                    },
                    WorkflowGraphWatchEvent {
                        cursor: 12,
                        event: complete.clone(),
                    },
                ],
            ),
        )
        .expect("combined page");
    uninterrupted.apply_page(page).expect("combined view page");

    let mut reconnect_state = state;
    reconnect_state
        .apply(11, &active)
        .expect("state RPC baseline at reconnect cursor");
    let mut reconnected_adapter = WorkflowGraphRpcAdapter::default();
    let mut reconnected = WorkflowGraphProjection::default();
    reconnected
        .replace(
            reconnected_adapter
                .replace(reconnect_state)
                .expect("fresh L2 reconnect baseline"),
        )
        .expect("fresh view baseline");
    let second = reconnected_adapter
        .apply_page(
            11,
            watch(
                11,
                12,
                vec![WorkflowGraphWatchEvent {
                    cursor: 12,
                    event: complete,
                }],
            ),
        )
        .expect("reconnected suffix");
    reconnected.apply_page(second).expect("suffix view");

    assert_eq!(reconnected, uninterrupted);
}

#[test]
fn empty_sparse_page_can_advance_before_the_next_graph_event_without_a_gap() {
    let (state, input, _) = graph();
    let active = activated(&state, input);

    let mut uninterrupted_adapter = WorkflowGraphRpcAdapter::default();
    let mut uninterrupted = WorkflowGraphProjection::default();
    uninterrupted
        .replace(
            uninterrupted_adapter
                .replace(state.clone())
                .expect("valid baseline"),
        )
        .expect("valid projection");
    let combined = uninterrupted_adapter
        .apply_page(
            10,
            watch(
                10,
                20,
                vec![WorkflowGraphWatchEvent {
                    cursor: 20,
                    event: active.clone(),
                }],
            ),
        )
        .expect("combined sparse page");
    uninterrupted
        .apply_page(combined)
        .expect("combined projection page");

    let mut sparse_adapter = WorkflowGraphRpcAdapter::default();
    let mut sparse = WorkflowGraphProjection::default();
    sparse
        .replace(sparse_adapter.replace(state).expect("valid baseline"))
        .expect("valid projection");
    let empty = sparse_adapter
        .apply_page(10, watch(10, 15, Vec::new()))
        .expect("unrelated session facts advance the watch cursor");
    sparse.apply_page(empty).expect("empty sparse page");
    assert_eq!(sparse.cursor(), Some(15));
    let suffix = sparse_adapter
        .apply_page(
            15,
            watch(
                15,
                20,
                vec![WorkflowGraphWatchEvent {
                    cursor: 20,
                    event: active,
                }],
            ),
        )
        .expect("later graph event follows the sparse cursor");
    sparse.apply_page(suffix).expect("sparse suffix");

    assert_eq!(sparse, uninterrupted);
    assert_eq!(sparse_adapter, uninterrupted_adapter);
}

#[test]
fn rejected_l2_node_keeps_openable_evidence_reference() {
    let (state, input, _) = graph();
    let rejection_parent = input.evidence.artifact.clone();
    let active = activated(&state, input);
    let rejection_evidence = evidence('c', "failure", vec![rejection_parent]);
    let rejected = WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
        graph_id: state.graph_id.clone(),
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 1,
        code: WorkflowNodeRejectCode::EvidenceRejected,
        message: "verification rejected the evidence".to_owned(),
        evidence: Some(rejection_evidence.clone()),
        convergence_gate: false,
    });
    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(adapter.replace(state).expect("valid L2 baseline"))
        .expect("valid baseline");
    let page = adapter
        .apply_page(
            10,
            watch(
                10,
                12,
                vec![
                    WorkflowGraphWatchEvent {
                        cursor: 11,
                        event: active,
                    },
                    WorkflowGraphWatchEvent {
                        cursor: 12,
                        event: rejected,
                    },
                ],
            ),
        )
        .expect("rejection page");
    projection.apply_page(page).expect("rejection view");

    let refs = projection
        .rejection_evidence("PLAN")
        .expect("rejected node is inspectable");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].as_str(), rejection_evidence.artifact.as_str());
}

#[test]
fn evidence_less_l2_rejection_keeps_typed_inspection_detail() {
    let (state, input, _) = graph();
    let active = activated(&state, input);
    let rejected = WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
        graph_id: state.graph_id.clone(),
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 1,
        code: WorkflowNodeRejectCode::Abandoned,
        message: "operator abandoned this activation".to_owned(),
        evidence: None,
        convergence_gate: false,
    });
    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(adapter.replace(state).expect("valid L2 baseline"))
        .expect("valid projection baseline");
    let page = adapter
        .apply_page(
            10,
            watch(
                10,
                12,
                vec![
                    WorkflowGraphWatchEvent {
                        cursor: 11,
                        event: active,
                    },
                    WorkflowGraphWatchEvent {
                        cursor: 12,
                        event: rejected,
                    },
                ],
            ),
        )
        .expect("valid evidence-less rejection");
    projection.apply_page(page).expect("valid rejection view");

    let rejection = projection
        .rejection("PLAN")
        .expect("evidence-less rejection remains inspectable");
    assert_eq!(rejection.code, WorkflowNodeRejectCode::Abandoned);
    assert_eq!(rejection.message, "operator abandoned this activation");
    assert_eq!(rejection.cursor, 12);
    assert!(rejection.evidence.is_none());
}

#[test]
fn back_edge_watch_event_lights_retry_input_and_activates_target() {
    let (mut state, plan_input, plan_output) = graph();
    let plan_activation = activated(&state, plan_input);
    state.apply(11, &plan_activation).expect("plan activates");
    let plan_completion = completed(&state, plan_output.clone());
    state.apply(12, &plan_completion).expect("plan completes");
    let verify_input = WorkflowNodeInput {
        edge_id: 2,
        evidence: plan_output.clone(),
    };
    let verify_activation =
        WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
            graph_id: state.graph_id.clone(),
            node: GraphNodeName::new("VERIFY").expect("valid node"),
            iteration: 1,
            activation_order: 2,
            cause: WorkflowActivationCause::ForwardJoin,
            input_ledger_digest: workflow_input_ledger_digest(std::slice::from_ref(&verify_input)),
            inputs: vec![verify_input],
        });
    state
        .apply(13, &verify_activation)
        .expect("verify activates");
    let retry_evidence = evidence('c', "brief", vec![plan_output.artifact]);
    let rejection = WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
        graph_id: state.graph_id.clone(),
        node: GraphNodeName::new("VERIFY").expect("valid node"),
        iteration: 1,
        code: WorkflowNodeRejectCode::EvidenceRejected,
        message: "retry plan with rejected evidence".to_owned(),
        evidence: Some(retry_evidence.clone()),
        convergence_gate: false,
    });
    state.apply(14, &rejection).expect("verify rejects");

    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(
            adapter
                .replace(state.clone())
                .expect("valid retry baseline"),
        )
        .expect("valid retry projection");
    let back_input = WorkflowNodeInput {
        edge_id: 3,
        evidence: retry_evidence,
    };
    let reactivated = WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
        graph_id: state.graph_id,
        node: GraphNodeName::new("PLAN").expect("valid node"),
        iteration: 2,
        activation_order: 3,
        cause: WorkflowActivationCause::BackEdge,
        input_ledger_digest: workflow_input_ledger_digest(std::slice::from_ref(&back_input)),
        inputs: vec![back_input],
    });
    let page = adapter
        .apply_page(
            14,
            watch(
                14,
                15,
                vec![WorkflowGraphWatchEvent {
                    cursor: 15,
                    event: reactivated,
                }],
            ),
        )
        .expect("valid back-edge suffix");
    projection.apply_page(page).expect("valid back-edge view");
    let plan = projection.node("PLAN").expect("projected plan");

    assert_eq!(plan.inputs_present, vec![true, true]);
    assert_eq!(plan.status, WorkflowNodeState::Active);
}

#[test]
fn adapter_refuses_a_desynced_cursor_without_mutating_its_baseline() {
    let (state, _, _) = graph();
    let mut adapter = WorkflowGraphRpcAdapter::default();
    adapter.replace(state).expect("valid L2 baseline");
    let before = adapter.clone();

    let error = adapter
        .apply_page(11, watch(11, 11, Vec::new()))
        .expect_err("view cursor may not skip beyond the retained L2 state");

    assert_eq!(adapter, before);
    assert!(
        error
            .to_string()
            .contains("baseline 10, watch requested 11")
    );
}

#[test]
fn wholly_stale_watch_page_is_an_idempotent_duplicate() {
    let (state, input, _) = graph();
    let event = activated(&state, input);
    let duplicate = watch(10, 11, vec![WorkflowGraphWatchEvent { cursor: 11, event }]);
    let mut adapter = WorkflowGraphRpcAdapter::default();
    let mut projection = WorkflowGraphProjection::default();
    projection
        .replace(adapter.replace(state).expect("valid baseline"))
        .expect("valid projection");
    let first = adapter
        .apply_page(10, duplicate.clone())
        .expect("first delivery");
    projection.apply_page(first).expect("first reduction");
    let before_adapter = adapter.clone();
    let before_projection = projection.clone();

    let stale = adapter
        .apply_page(11, duplicate)
        .expect("duplicate delivery is tolerated");
    assert!(!projection.apply_page(stale).expect("duplicate projection"));

    assert_eq!(adapter, before_adapter);
    assert_eq!(projection, before_projection);
}

#[test]
fn malformed_rpc_page_metadata_is_atomic() {
    let (state, _, _) = graph();
    let mut adapter = WorkflowGraphRpcAdapter::default();
    adapter.replace(state).expect("valid L2 baseline");
    let before = adapter.clone();

    let error = adapter
        .apply_page(
            10,
            WorkflowGraphWatchPage {
                requested_after_cursor: 10,
                replay_through_cursor: 10,
                next_cursor: 11,
                events: Vec::new(),
            },
        )
        .expect_err("next cursor beyond replay bound must fail");

    assert_eq!(adapter, before);
    assert!(error.to_string().contains("cursor bounds are inconsistent"));
}

#[test]
fn next_cursor_must_name_the_nonempty_page_tail() {
    let (state, input, _) = graph();
    let active = activated(&state, input);
    let mut adapter = WorkflowGraphRpcAdapter::default();
    adapter.replace(state).expect("valid L2 baseline");
    let before = adapter.clone();

    let error = adapter
        .apply_page(
            10,
            WorkflowGraphWatchPage {
                requested_after_cursor: 10,
                replay_through_cursor: 12,
                next_cursor: 12,
                events: vec![WorkflowGraphWatchEvent {
                    cursor: 11,
                    event: active,
                }],
            },
        )
        .expect_err("next cursor may not skip past the returned page tail");

    assert_eq!(adapter, before);
    assert!(error.to_string().contains("does not match the page tail"));
}
