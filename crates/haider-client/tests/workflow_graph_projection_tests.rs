//! v0.0.963 L3 workflow runtime projection laws.

#![allow(clippy::expect_used)]

use haider_client::{
    WorkflowEvidenceRef, WorkflowGraphChange, WorkflowGraphEdge, WorkflowGraphEdgeKind,
    WorkflowGraphProjection, WorkflowGraphState, WorkflowGraphWatchPage, WorkflowNodeProjection,
    WorkflowNodeRejection, WorkflowNodeState,
};
use haider_protocol::graph::WorkflowNodeRejectCode;
use haider_protocol::ids::ArtifactRef;

fn artifact(byte: char) -> ArtifactRef {
    ArtifactRef::new(format!("blake3:{}", byte.to_string().repeat(64)))
}

fn node(id: &str, status: WorkflowNodeState, inputs: &[bool]) -> WorkflowNodeProjection {
    WorkflowNodeProjection {
        node_id: id.to_owned(),
        status,
        inputs_present: inputs.to_vec(),
        evidence_refs: Vec::new(),
        rejection: None,
    }
}

fn state(cursor: u64) -> WorkflowGraphState {
    WorkflowGraphState {
        graph_id: "graph-release-1".to_owned(),
        workflow_id: "release".to_owned(),
        workflow_digest: "blake3:release-v1".to_owned(),
        cursor,
        nodes: vec![
            node("PLAN", WorkflowNodeState::Active, &[true]),
            node("BUILD", WorkflowNodeState::Waiting, &[false]),
            node("DOCS", WorkflowNodeState::Waiting, &[false]),
            node("VERIFY", WorkflowNodeState::Waiting, &[false, false]),
        ],
        edges: vec![
            WorkflowGraphEdge {
                kind: WorkflowGraphEdgeKind::GraphInput,
                from: None,
                to: "PLAN".to_owned(),
            },
            WorkflowGraphEdge {
                kind: WorkflowGraphEdgeKind::Forward,
                from: Some("PLAN".to_owned()),
                to: "BUILD".to_owned(),
            },
            WorkflowGraphEdge {
                kind: WorkflowGraphEdgeKind::Forward,
                from: Some("PLAN".to_owned()),
                to: "DOCS".to_owned(),
            },
            WorkflowGraphEdge {
                kind: WorkflowGraphEdgeKind::Forward,
                from: Some("BUILD".to_owned()),
                to: "VERIFY".to_owned(),
            },
            WorkflowGraphEdge {
                kind: WorkflowGraphEdgeKind::Forward,
                from: Some("DOCS".to_owned()),
                to: "VERIFY".to_owned(),
            },
        ],
    }
}

fn change(
    cursor: u64,
    id: &str,
    status: WorkflowNodeState,
    inputs: &[bool],
) -> WorkflowGraphChange {
    WorkflowGraphChange {
        cursor,
        node: node(id, status, inputs),
    }
}

fn page(
    requested_after_cursor: u64,
    replay_through_cursor: u64,
    next_cursor: u64,
    changes: Vec<WorkflowGraphChange>,
) -> WorkflowGraphWatchPage {
    WorkflowGraphWatchPage {
        graph_id: "graph-release-1".to_owned(),
        workflow_id: "release".to_owned(),
        workflow_digest: "blake3:release-v1".to_owned(),
        requested_after_cursor,
        replay_through_cursor,
        next_cursor,
        changes,
    }
}

#[test]
fn watch_stream_reduces_to_expected_node_states() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    projection
        .apply_page(page(
            40,
            48,
            48,
            vec![
                change(41, "PLAN", WorkflowNodeState::Complete, &[true]),
                change(43, "BUILD", WorkflowNodeState::Ready, &[true]),
                // One completion cursor can update multiple displayed nodes:
                // BUILD completes its typed output while DOCS becomes ready.
                change(43, "DOCS", WorkflowNodeState::Ready, &[true]),
                change(46, "BUILD", WorkflowNodeState::Active, &[true]),
                change(47, "BUILD", WorkflowNodeState::Complete, &[true]),
                change(48, "VERIFY", WorkflowNodeState::Waiting, &[true, false]),
            ],
        ))
        .expect("valid sparse watch page");

    assert_eq!(projection.cursor(), Some(48));
    assert_eq!(
        projection.node("PLAN").map(|node| node.status),
        Some(WorkflowNodeState::Complete)
    );
    assert_eq!(
        projection.node("DOCS").map(|node| node.status),
        Some(WorkflowNodeState::Ready)
    );
    let verify = projection.node("VERIFY").expect("verify node");
    assert_eq!(verify.present_input_count(), 1);
    assert_eq!(verify.inputs_present.len(), 2);
    assert!(!verify.all_inputs_present());
}

#[test]
fn reconnect_from_applied_cursor_yields_identical_state() {
    let first_page = page(
        40,
        44,
        44,
        vec![
            change(41, "PLAN", WorkflowNodeState::Complete, &[true]),
            change(44, "BUILD", WorkflowNodeState::Active, &[true]),
        ],
    );
    let second_page = page(
        44,
        49,
        49,
        vec![
            change(47, "BUILD", WorkflowNodeState::Complete, &[true]),
            change(49, "DOCS", WorkflowNodeState::Rejected, &[true]),
        ],
    );

    let mut uninterrupted = WorkflowGraphProjection::default();
    uninterrupted.replace(state(40)).expect("valid state");
    uninterrupted
        .apply_page(first_page.clone())
        .expect("first page");
    uninterrupted
        .apply_page(second_page.clone())
        .expect("second page");

    let mut reconnected = WorkflowGraphProjection::default();
    reconnected.replace(state(40)).expect("valid state");
    reconnected
        .apply_page(first_page)
        .expect("prefix before disconnect");
    assert_eq!(reconnected.cursor(), Some(44));
    reconnected
        .apply_page(second_page)
        .expect("replayed suffix");

    assert_eq!(reconnected, uninterrupted);
    assert_eq!(reconnected.cursor(), Some(49));
}

#[test]
fn rejected_node_exposes_its_evidence_refs() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    let mut rejected = node("DOCS", WorkflowNodeState::Rejected, &[true]);
    rejected.evidence_refs = vec![
        WorkflowEvidenceRef::new(artifact('c')),
        WorkflowEvidenceRef::new(artifact('d')),
    ];
    rejected.rejection = Some(WorkflowNodeRejection {
        code: WorkflowNodeRejectCode::EvidenceRejected,
        message: "docs verification failed".to_owned(),
        cursor: 43,
        evidence: Some(WorkflowEvidenceRef::new(artifact('c'))),
    });
    projection
        .apply_page(page(
            40,
            43,
            43,
            vec![WorkflowGraphChange {
                cursor: 43,
                node: rejected,
            }],
        ))
        .expect("rejection update");

    let evidence = projection
        .rejection_evidence("DOCS")
        .expect("rejected node is inspectable");
    assert_eq!(evidence.len(), 2);
    assert_eq!(evidence[0].as_str(), artifact('c').as_str());
    assert!(projection.rejection_evidence("PLAN").is_none());
}

#[test]
fn a_page_cursor_mismatch_never_partially_applies_the_suffix() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    let before = projection.clone();
    let error = projection
        .apply_page(page(
            41,
            42,
            42,
            vec![change(42, "PLAN", WorkflowNodeState::Complete, &[true])],
        ))
        .expect_err("mismatched request cursor must reconnect");

    assert_eq!(projection, before);
    assert_eq!(projection.cursor(), Some(40));
    assert!(
        error
            .to_string()
            .contains("requested cursor 41, applied cursor is 40")
    );
}

#[test]
fn a_second_run_of_the_same_workflow_requires_a_new_graph_baseline() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    let before = projection.clone();
    let mut other_run = page(
        40,
        41,
        41,
        vec![change(41, "PLAN", WorkflowNodeState::Complete, &[true])],
    );
    other_run.graph_id = "graph-release-2".to_owned();

    let error = projection
        .apply_page(other_run)
        .expect_err("same workflow id cannot alias another graph run");

    assert_eq!(projection, before);
    assert!(error.to_string().contains("graph-release-2"));
}

#[test]
fn clearing_a_missing_state_removes_its_cursor_and_nodes() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");

    projection.clear();

    assert!(projection.is_empty());
    assert_eq!(projection.cursor(), None);
    assert_eq!(projection.workflow_id(), None);
}

#[test]
fn an_exact_cursor_noop_page_cannot_hide_malformed_changes() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    let before = projection.clone();

    let error = projection
        .apply_page(page(
            40,
            40,
            40,
            vec![change(41, "PLAN", WorkflowNodeState::Complete, &[true])],
        ))
        .expect_err("a current-cursor page still validates its changes");

    assert_eq!(projection, before);
    assert!(error.to_string().contains("exceed the replay bound"));
}

#[test]
fn malformed_frozen_topology_never_replaces_the_last_good_graph() {
    let mut projection = WorkflowGraphProjection::default();
    projection.replace(state(40)).expect("valid state");
    let before = projection.clone();
    let mut malformed = state(50);
    malformed.edges.push(WorkflowGraphEdge {
        kind: WorkflowGraphEdgeKind::Forward,
        from: Some("MISSING".to_owned()),
        to: "VERIFY".to_owned(),
    });

    let error = projection
        .replace(malformed)
        .expect_err("edge source must be a runtime node");

    assert_eq!(projection, before);
    assert!(error.to_string().contains("edge source"));
}
