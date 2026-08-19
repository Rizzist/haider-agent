//! W-ROLL — the child workflow-run rollup projection: state vocabulary,
//! node tallies, gate naming, and the materiality/dedup laws that keep a
//! run of N nodes at O(N) published rollups.
#![allow(clippy::expect_used)]

use crate::delegation::{graph_rollup, rollup_is_material, same_rollup_transition};
use haider_protocol::EventPayload;
use haider_protocol::agent::AgentGraphRollupV1;
use haider_protocol::graph::{
    GraphEvidenceTally, GraphGateKind, GraphGateSatisfied, GraphNodeName, GraphNodeStatus,
    GraphPhase, GraphStatus,
};
use haider_protocol::ids::{AgentId, GraphId};

fn node(name: &str, satisfied: bool, gate: Option<GraphGateKind>) -> GraphNodeStatus {
    GraphNodeStatus {
        node: GraphNodeName::new(name).expect("node name"),
        gate,
        executor: None,
        attempts_opened: 1,
        current_attempt: Some(1),
        evidence: GraphEvidenceTally::default(),
        evidence_slots: Vec::new(),
        satisfied,
    }
}

fn status(phase: GraphPhase, current: Option<&str>, nodes: Vec<GraphNodeStatus>) -> GraphStatus {
    GraphStatus {
        graph_id: GraphId::new("graph-roll"),
        template: "ship".into(),
        digest: "d1".into(),
        template_version: 1,
        start_node: None,
        phase,
        current_node: current.map(|name| GraphNodeName::new(name).expect("node name")),
        ready_nodes: Vec::new(),
        attempt: 1,
        nodes,
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    }
}

fn agent() -> AgentId {
    AgentId::new("wf-child")
}

/// MUTATION CHECK: flip the phase→state table (Completed no longer says
/// "complete", a terminal child on an Active graph no longer fails), break
/// the 1-based node ordinal, or count unsatisfied nodes as green. Expected
/// RUNTIME failure: a row of this projection table changes.
#[test]
fn rollup_state_vocabulary_and_tallies_project_the_graph() {
    let running = graph_rollup(
        &agent(),
        &status(
            GraphPhase::Active,
            Some("BUILD"),
            vec![
                node("PLAN", true, None),
                node("BUILD", false, Some(GraphGateKind::CommandGreen)),
                node("SHIP", false, Some(GraphGateKind::HumanConfirm)),
            ],
        ),
        None,
        false,
    )
    .expect("projects");
    assert_eq!(running.state, "running");
    assert_eq!(running.node_index, 2, "1-based current ordinal");
    assert_eq!(running.nodes_total, 3);
    assert_eq!(running.nodes_green, 1);
    assert_eq!(running.gate, None, "running carries no gate word");
    assert_eq!(
        running.workflow_id, None,
        "no registry join without a workflow"
    );
    assert_eq!(running.node_label.as_deref(), Some("BUILD"));

    let complete = graph_rollup(
        &agent(),
        &status(
            GraphPhase::Completed,
            None,
            vec![node("PLAN", true, None), node("SHIP", true, None)],
        ),
        None,
        false,
    )
    .expect("projects");
    assert_eq!(complete.state, "complete");
    assert_eq!(complete.nodes_green, 2);

    // A terminalized child run on a still-Active graph is a FAILED run —
    // the graph never converged.
    let failed = graph_rollup(
        &agent(),
        &status(
            GraphPhase::Active,
            Some("BUILD"),
            vec![node("PLAN", true, None), node("BUILD", false, None)],
        ),
        None,
        true,
    )
    .expect("projects");
    assert_eq!(failed.state, "failed");
}

/// MUTATION CHECK: stop requiring a PENDING menu for the gate state, or
/// lose the human gate word. Expected RUNTIME failure: an idle human gate
/// reads as "gate" without a confirm to answer, or the word changes.
#[test]
fn gate_state_requires_a_pending_menu_and_names_the_gate() {
    let mut waiting = status(
        GraphPhase::Active,
        Some("SHIP"),
        vec![
            node("PLAN", true, None),
            node("SHIP", false, Some(GraphGateKind::HumanConfirm)),
        ],
    );
    // No pending menu yet: the walker has not opened the confirm — running.
    let before = graph_rollup(&agent(), &waiting, None, false).expect("projects");
    assert_eq!(before.state, "running");

    waiting.pending_menu = Some(haider_protocol::ids::MenuId::new("menu-1"));
    let gated = graph_rollup(&agent(), &waiting, None, false).expect("projects");
    assert_eq!(gated.state, "gate");
    assert_eq!(gated.gate.as_deref(), Some("human"));
}

/// MUTATION CHECK: drop a field from the transition identity, or invert the
/// materiality suppression for evidence-only gate ticks. Expected RUNTIME
/// failure: duplicate rollups publish (or real transitions are swallowed).
#[test]
fn dedup_and_materiality_bound_publishes_to_transitions() {
    let base = AgentGraphRollupV1 {
        agent: agent(),
        workflow_id: Some("ship".into()),
        template_digest: "d1".into(),
        state: "running".into(),
        node_index: 2,
        nodes_total: 3,
        nodes_green: 1,
        node_label: Some("BUILD".into()),
        agent_type: None,
        gate: None,
    };
    assert!(same_rollup_transition(Some(&base), &base.clone()));
    let mut advanced = base.clone();
    advanced.node_index = 3;
    assert!(!same_rollup_transition(Some(&base), &advanced));
    let mut greener = base.clone();
    greener.nodes_green = 2;
    assert!(!same_rollup_transition(Some(&base), &greener));
    assert!(!same_rollup_transition(None, &base));

    // A GateSatisfied tick that leaves the run on the SAME node while still
    // running is immaterial; the same payload carrying a node advance is not.
    let tick = EventPayload::GraphGateSatisfied(GraphGateSatisfied {
        graph_id: GraphId::new("graph-roll"),
        node: GraphNodeName::new("BUILD").expect("node name"),
        attempt: 1,
    });
    assert!(!rollup_is_material(&tick, Some(&base), &base.clone()));
    assert!(rollup_is_material(&tick, Some(&base), &advanced));
    let mut completed = base.clone();
    completed.state = "complete".into();
    assert!(rollup_is_material(&tick, Some(&base), &completed));
}
