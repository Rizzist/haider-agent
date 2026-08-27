#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_protocol::graph::{
    GraphEvidenceTally, GraphGateKind, GraphNodeStatus, WorkflowActivationCause,
    WorkflowGraphJournalEvent, WorkflowGraphStarted, WorkflowGraphState, WorkflowNodeActivated,
    WorkflowNodeInput, workflow_activation_ast_digest, workflow_activation_ast_from_loom,
    workflow_input_ledger_digest,
};
use haider_protocol::ids::{ArtifactRef, GraphId};
use haider_protocol::loom::{LoomTypeSig, compile_pipe, parse_pipe};
use haider_protocol::pipe::InstructEvidenceRef;
use haider_protocol::typed_agent::{TypedAgentInstallProgress, TypedAgentInstallState};

fn record(clis: Vec<String>) -> LoomAgentType {
    LoomAgentType {
        id: "researcher".into(),
        name: "Researcher".into(),
        job: "Work only inside the research scope.".into(),
        in_type: "Question".into(),
        out_type: "Sources".into(),
        clis,
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: String::new(),
        glyph: String::new(),
        rev: 3,
    }
}

fn job(record: &LoomAgentType, state: TypedAgentInstallState) -> TypedAgentInstallJob {
    TypedAgentInstallJob {
        job_id: "typed-install:researcher:3".into(),
        agent_type_id: record.id.clone(),
        agent_type_rev: record.rev,
        agent_type_digest: record.digest(),
        state,
        progress: TypedAgentInstallProgress {
            total: 1,
            completed: u16::from(matches!(
                state,
                TypedAgentInstallState::Verifying | TypedAgentInstallState::Succeeded
            )),
            current_cli: matches!(state, TypedAgentInstallState::Installing).then(|| "rg".into()),
        },
        error: (state == TypedAgentInstallState::Failed).then(|| "install failed".into()),
        created_at_ms: 1,
        updated_at_ms: 2,
    }
}

fn activated_graph(workflow: &LoomWorkflow, graph_id: &GraphId) -> WorkflowGraphState {
    let ast = workflow_activation_ast_from_loom(workflow).expect("activation AST");
    let seed = InstructEvidenceRef::new(
        ArtifactRef::new(format!("blake3:{}", "a".repeat(64))),
        ast.input_type.clone(),
        1,
        Vec::new(),
    );
    let mut state = WorkflowGraphState::from_started(
        1,
        WorkflowGraphStarted {
            graph_id: graph_id.clone(),
            ast_digest: workflow_activation_ast_digest(&ast),
            ast,
            seed: Some(seed.clone()),
        },
    )
    .expect("started activation graph");
    let node = workflow.meta[0].node.clone();
    let edge_id = state.ast.nodes[0].join.initial_all[0];
    let inputs = vec![WorkflowNodeInput {
        edge_id,
        evidence: seed,
    }];
    state
        .apply(
            2,
            &WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
                graph_id: graph_id.clone(),
                node,
                iteration: 1,
                activation_order: 1,
                cause: WorkflowActivationCause::ForwardJoin,
                input_ledger_digest: workflow_input_ledger_digest(&inputs),
                inputs,
            }),
        )
        .expect("node activation");
    state
}

fn activation_input_payloads(state: &WorkflowGraphState) -> Vec<TypedWorkflowInputPayload> {
    let node = state
        .nodes
        .iter()
        .find(|node| node.phase == haider_protocol::graph::WorkflowNodePhase::Activated)
        .expect("activated node");
    node.inputs
        .iter()
        .map(|input| TypedWorkflowInputPayload {
            edge_id: input.edge_id,
            artifact: input.evidence.artifact.clone(),
            evidence_type: input.evidence.evidence_type.clone(),
            ledger_digest: input.evidence.ledger_digest.clone(),
            content: "{\"seed\":true}".into(),
        })
        .collect()
}

#[test]
fn required_cli_job_gates_dispatch_and_daemon_frames_role() {
    let record = record(vec!["rg".into()]);
    let pending = prepare_typed_dispatch(
        record.clone(),
        Some(job(&record, TypedAgentInstallState::Installing)),
        "@fake task",
        "find sources",
    )
    .expect_err("pending install must gate dispatch");
    assert_eq!(pending.code, "typed_agent_install_pending");

    let plan = prepare_typed_dispatch(
        record.clone(),
        Some(job(&record, TypedAgentInstallState::Succeeded)),
        "@fake task",
        "find sources",
    )
    .expect("installed type dispatches");
    assert_eq!(plan.contract.required_clis[0].program, "rg");
    assert_eq!(plan.task, "@researcher · fake task");
    assert!(plan.prompt.contains("Work only inside the research scope."));
}

#[test]
fn cli_free_type_needs_no_install_job() {
    let plan = prepare_typed_dispatch(record(Vec::new()), None, "task", "prompt")
        .expect("empty contract is immediately ready");
    assert!(plan.contract.required_clis.is_empty());
}

#[test]
fn pinned_open_node_selects_its_type_without_model_arguments() {
    let ast = parse_pipe("typed-flow: Question -> Sources\nresearch @researcher \"find sources\"");
    let mut workflow = compile_pipe(&ast, |id| {
        (id == "researcher").then(|| LoomTypeSig {
            in_type: "Question".into(),
            out_type: "Sources".into(),
        })
    })
    .expect("typed workflow");
    let selected_record = record(Vec::new());
    workflow.meta[0].agent_type_rev = Some(selected_record.rev);
    workflow.meta[0].agent_type_digest = Some(selected_record.digest());
    workflow.refresh_digest();
    let node = workflow.meta[0].node.clone();
    let status = GraphStatus {
        graph_id: GraphId::new("typed-flow-graph"),
        template: workflow.template.name.clone(),
        digest: graph_template_digest(&workflow.template),
        template_version: workflow.template.version,
        start_node: Some(node.clone()),
        phase: GraphPhase::Active,
        current_node: Some(node.clone()),
        ready_nodes: vec![node.clone()],
        attempt: 1,
        nodes: vec![GraphNodeStatus {
            node: node.clone(),
            gate: Some(GraphGateKind::CommandGreen),
            executor: None,
            attempts_opened: 1,
            current_attempt: Some(1),
            evidence: GraphEvidenceTally::default(),
            evidence_slots: Vec::new(),
            satisfied: false,
        }],
        blocked_reason: None,
        pending_menu: None,
        pending_menus: Vec::new(),
        run_set: None,
    };
    let activation = activated_graph(&workflow, &status.graph_id);
    let activation_inputs = activation_input_payloads(&activation);

    let selected = prepare_typed_workflow_node_dispatch(
        &workflow,
        &status,
        Some(&activation),
        &activation_inputs,
        selected_record.clone(),
        None,
    )
    .expect("daemon binding")
    .expect("typed node");
    assert_eq!(selected.node, node);
    assert_eq!(selected.dispatch.record.id, "researcher");
    assert!(
        selected
            .dispatch
            .prompt
            .contains("Daemon-selected Loom node")
    );
    assert!(
        selected.dispatch.prompt.contains("data={\"seed\":true}"),
        "the executor receives the CAS-verified evidence body, not only its address"
    );

    for phase in [
        haider_protocol::graph::WorkflowNodePhase::Waiting,
        haider_protocol::graph::WorkflowNodePhase::Rejected,
    ] {
        let mut inactive = activation.clone();
        inactive.nodes[0].phase = phase;
        let refusal = prepare_typed_workflow_node_dispatch(
            &workflow,
            &status,
            Some(&inactive),
            &activation_inputs,
            selected_record.clone(),
            None,
        )
        .expect_err("a legacy-ready typed node cannot bypass its activation phase");
        assert_eq!(refusal.code, "typed_workflow_node_not_activated");
    }

    let mut impostor = record(Vec::new());
    impostor.id = "impostor".into();
    let refusal = prepare_typed_workflow_node_dispatch(
        &workflow,
        &status,
        Some(&activation),
        &activation_inputs,
        impostor,
        None,
    )
    .expect_err("workflow metadata rejects type substitution");
    assert_eq!(refusal.code, "typed_workflow_type_mismatch");

    let mut revised = selected_record;
    revised.rev = revised.rev.saturating_add(1);
    revised.job = "A newly revised role contract.".into();
    let refusal = prepare_typed_workflow_node_dispatch(
        &workflow,
        &status,
        Some(&activation),
        &activation_inputs,
        revised,
        None,
    )
    .expect_err("pinned node rejects a changed registry contract");
    assert_eq!(refusal.code, "typed_workflow_contract_stale");
}
