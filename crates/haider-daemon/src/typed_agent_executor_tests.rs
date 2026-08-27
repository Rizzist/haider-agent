#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use haider_protocol::graph::{GraphEvidenceTally, GraphGateKind, GraphNodeStatus};
use haider_protocol::ids::GraphId;
use haider_protocol::loom::{LoomTypeSig, compile_pipe, parse_pipe};
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
        denials: Vec::new(),
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

    let selected =
        prepare_typed_workflow_node_dispatch(&workflow, &status, selected_record.clone(), None)
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

    let mut impostor = record(Vec::new());
    impostor.id = "impostor".into();
    let refusal = prepare_typed_workflow_node_dispatch(&workflow, &status, impostor, None)
        .expect_err("workflow metadata rejects type substitution");
    assert_eq!(refusal.code, "typed_workflow_type_mismatch");

    let mut revised = selected_record;
    revised.rev = revised.rev.saturating_add(1);
    revised.job = "A newly revised role contract.".into();
    let refusal = prepare_typed_workflow_node_dispatch(&workflow, &status, revised, None)
        .expect_err("pinned node rejects a changed registry contract");
    assert_eq!(refusal.code, "typed_workflow_contract_stale");
}
