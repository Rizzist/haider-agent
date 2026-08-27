#![allow(clippy::expect_used)]

use super::{
    TypedWorkflowExecutionBinding, default_child_grant, loom_provider_grant,
    typed_workflow_coordinates_match, typed_workflow_node_grant, validate_grant,
};
use haider_protocol::agent::Grant;
use haider_protocol::effect::EffectClass;
use haider_protocol::graph::{GraphNodeName, GraphPhase};
use haider_protocol::ids::GraphId;
use haider_protocol::loom::LoomAgentType;

#[test]
fn typed_executor_binding_ends_when_node_or_attempt_advances() {
    let graph_id = GraphId::new("typed-graph");
    let node = GraphNodeName::new("RESEARCH").expect("node");
    let binding = TypedWorkflowExecutionBinding {
        graph_id: graph_id.clone(),
        node: node.clone(),
        attempt: 2,
        agent_type_id: "researcher".into(),
    };
    assert!(typed_workflow_coordinates_match(
        &binding,
        &graph_id,
        GraphPhase::Active,
        Some(&node),
        true,
        Some(2),
        false,
    ));

    let next_node = GraphNodeName::new("VERIFY").expect("next node");
    assert!(!typed_workflow_coordinates_match(
        &binding,
        &graph_id,
        GraphPhase::Active,
        Some(&next_node),
        true,
        Some(2),
        false,
    ));
    assert!(!typed_workflow_coordinates_match(
        &binding,
        &graph_id,
        GraphPhase::Active,
        Some(&node),
        true,
        Some(3),
        false,
    ));
    assert!(!typed_workflow_coordinates_match(
        &binding,
        &GraphId::new("replacement-graph"),
        GraphPhase::Active,
        Some(&node),
        true,
        Some(2),
        false,
    ));

    // Completion may clear the volatile graph brief, but no subsequent
    // tool execution is allowed under the finished specialist binding.
    assert!(typed_workflow_coordinates_match(
        &binding,
        &graph_id,
        GraphPhase::Completed,
        None,
        false,
        None,
        true,
    ));
    assert!(!typed_workflow_coordinates_match(
        &binding,
        &graph_id,
        GraphPhase::Completed,
        None,
        false,
        None,
        false,
    ));
}

#[test]
fn native_typed_workflow_on_generic_child_fails_closed_without_graph_evidence() {
    let record = LoomAgentType {
        id: "researcher".into(),
        name: "Researcher".into(),
        job: "Research only the scoped node.".into(),
        in_type: "Question".into(),
        out_type: "Sources".into(),
        clis: Vec::new(),
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
        rev: 1,
    };
    let node = GraphNodeName::new("RESEARCH").expect("node");
    let inherited = default_child_grant();
    let error = typed_workflow_node_grant(&record, &node, Some(&inherited))
        .expect_err("native pin must not widen a generic child's grant");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::PermissionDenied
    );

    let mut workflow_child = inherited;
    workflow_child.tools.push("graph_evidence".into());
    assert!(typed_workflow_node_grant(&record, &node, Some(&workflow_child)).is_ok());
}

#[test]
fn loom_static_provider_ceiling_excludes_actor_owned_and_delegation_tools() {
    let grant = loom_provider_grant(None);
    for denied in [
        "request_input",
        "plan",
        "todo_write",
        "spawn_subagent",
        "message_subagent",
        "workflow_author",
    ] {
        assert!(!grant.tools.iter().any(|tool| tool == denied), "{denied}");
    }
    for brokered in ["graph_evidence", "fs_read", "process_exec", "web_fetch"] {
        assert!(
            grant.tools.iter().any(|tool| tool == brokered),
            "{brokered}"
        );
    }
}

#[test]
fn loom_provider_ceiling_preserves_inherited_network_host_scopes() {
    let inherited = Grant {
        tools: vec!["web_fetch".into(), "graph_evidence".into()],
        effect_ceiling: vec![EffectClass::Network {
            host: "api.example.test".into(),
        }],
    };
    let grant = loom_provider_grant(Some(&inherited));
    assert!(grant.tools.iter().any(|tool| tool == "web_fetch"));
    assert_eq!(
        grant.effect_ceiling,
        vec![EffectClass::Network {
            host: "api.example.test".into(),
        }]
    );
    validate_grant(&grant).expect("host-scoped Loom provider grant remains coherent");
}
