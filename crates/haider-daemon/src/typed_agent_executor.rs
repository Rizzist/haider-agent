//! Daemon-owned admission and role framing for typed-agent executions.
//!
//! A model may request `agent_type`, but it cannot manufacture the resulting
//! role, CLI scope, or readiness decision. This boundary freezes those facts
//! from the registry revision and its durable install job before delegation
//! creates a child session.

use haider_protocol::graph::{GraphNodeName, GraphPhase, GraphStatus, graph_template_digest};
use haider_protocol::loom::{LoomAgentType, LoomWorkflow};
use haider_protocol::typed_agent::{
    TypedAgentContract, TypedAgentInstallJob, TypedAgentInstallState,
};

#[derive(Debug, Clone)]
pub(crate) struct TypedAgentDispatchPlan {
    pub(crate) record: LoomAgentType,
    pub(crate) contract: TypedAgentContract,
    pub(crate) task: String,
    pub(crate) prompt: String,
}

/// One current Loom node bound by daemon graph state, not by model-authored
/// spawn arguments. The worker applies this role and capability scope to the
/// provider turn that executes the OPEN node.
#[derive(Debug, Clone)]
pub(crate) struct TypedWorkflowNodeDispatchPlan {
    pub(crate) workflow_id: String,
    pub(crate) node: GraphNodeName,
    pub(crate) dispatch: TypedAgentDispatchPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypedAgentDispatchRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) install_job: Option<TypedAgentInstallJob>,
}

/// Freeze one typed dispatch from daemon-owned registry and installer state.
/// Required CLIs are a hard readiness gate: PATH presence alone cannot bypass
/// a missing, pending, failed, or stale durable install job.
pub(crate) fn prepare_typed_dispatch(
    record: LoomAgentType,
    install_job: Option<TypedAgentInstallJob>,
    requested_task: &str,
    requested_prompt: &str,
) -> Result<TypedAgentDispatchPlan, TypedAgentDispatchRefusal> {
    let contract = TypedAgentContract::from_loom_agent_type(&record).map_err(|error| {
        TypedAgentDispatchRefusal {
            code: "typed_agent_contract_invalid",
            message: error.message,
            install_job: None,
        }
    })?;

    if !contract.required_clis.is_empty() {
        let Some(job) = install_job else {
            return Err(TypedAgentDispatchRefusal {
                code: "typed_agent_install_missing",
                message: format!(
                    "typed agent @{} revision {} has no durable required-CLI install job",
                    contract.agent_type_id, contract.agent_type_rev
                ),
                install_job: None,
            });
        };
        if job.agent_type_id != contract.agent_type_id
            || job.agent_type_rev != contract.agent_type_rev
            || job.agent_type_digest != contract.agent_type_digest
        {
            return Err(TypedAgentDispatchRefusal {
                code: "typed_agent_install_stale",
                message: format!(
                    "typed agent @{} revision {} is not covered by install job {}",
                    contract.agent_type_id, contract.agent_type_rev, job.job_id
                ),
                install_job: Some(job),
            });
        }
        if let Err(error) = job.validate() {
            return Err(TypedAgentDispatchRefusal {
                code: "typed_agent_install_invalid",
                message: error.message,
                install_job: Some(job),
            });
        }
        match job.state {
            TypedAgentInstallState::Succeeded => {}
            TypedAgentInstallState::Failed => {
                return Err(TypedAgentDispatchRefusal {
                    code: "typed_agent_install_failed",
                    message: job.error.clone().unwrap_or_else(|| {
                        format!("required-CLI installation {} failed", job.job_id)
                    }),
                    install_job: Some(job),
                });
            }
            state => {
                return Err(TypedAgentDispatchRefusal {
                    code: "typed_agent_install_pending",
                    message: format!(
                        "required-CLI installation {} is {state:?} ({}/{})",
                        job.job_id, job.progress.completed, job.progress.total
                    ),
                    install_job: Some(job),
                });
            }
        }
    }

    // The role header is daemon truth. Registry newlines are flattened only
    // in display fields; scoped instructions retain their authored structure.
    let one_line = |value: &str| value.replace(['\n', '\r'], " ");
    let prompt = format!(
        "[agent type @{} — {} · {} -> {}]\n{}\n\n{}",
        contract.role.scope,
        one_line(&contract.role.name),
        one_line(&record.in_type),
        one_line(&record.out_type),
        contract.role.instructions,
        requested_prompt
    );
    let clean_task = requested_task
        .trim_start_matches('@')
        .trim_start()
        .to_owned();
    Ok(TypedAgentDispatchPlan {
        task: format!("@{} · {clean_task}", contract.agent_type_id),
        prompt,
        record,
        contract,
    })
}

/// Resolve the specialist for the pinned workflow's current ready node. A
/// caller cannot omit or substitute `agent_type`: graph status chooses the
/// node, immutable Loom metadata chooses the type, and the registry/install
/// snapshot only proves that exact contract is executable.
pub(crate) fn prepare_typed_workflow_node_dispatch(
    workflow: &LoomWorkflow,
    status: &GraphStatus,
    record: LoomAgentType,
    install_job: Option<TypedAgentInstallJob>,
) -> Result<Option<TypedWorkflowNodeDispatchPlan>, TypedAgentDispatchRefusal> {
    if status.phase != GraphPhase::Active
        || status.template != workflow.template.name
        || status.digest != graph_template_digest(&workflow.template)
    {
        return Ok(None);
    }
    let Some(node) = status.current_node.as_ref() else {
        return Ok(None);
    };
    if !status.node_is_ready(node) {
        return Ok(None);
    }
    let Some(meta) = workflow.meta.iter().find(|meta| &meta.node == node) else {
        return Err(TypedAgentDispatchRefusal {
            code: "typed_workflow_node_missing",
            message: format!(
                "pinned Loom workflow {} has no metadata for current node {node}",
                workflow.id
            ),
            install_job: None,
        });
    };
    let Some(expected_type) = meta.agent_type.as_deref() else {
        return Ok(None);
    };
    if record.id != expected_type {
        return Err(TypedAgentDispatchRefusal {
            code: "typed_workflow_type_mismatch",
            message: format!(
                "current Loom node {node} requires @{expected_type}, not @{}",
                record.id
            ),
            install_job: None,
        });
    }
    let (Some(expected_rev), Some(expected_digest)) =
        (meta.agent_type_rev, meta.agent_type_digest.as_deref())
    else {
        return Err(TypedAgentDispatchRefusal {
            code: "typed_workflow_contract_unbound",
            message: format!(
                "current Loom node {node} has no frozen agent contract; re-register and re-pin the workflow"
            ),
            install_job: None,
        });
    };
    if record.rev != expected_rev || record.digest() != expected_digest {
        return Err(TypedAgentDispatchRefusal {
            code: "typed_workflow_contract_stale",
            message: format!(
                "current Loom node {node} pins @{expected_type} revision {expected_rev}, but the registry contract changed"
            ),
            install_job: None,
        });
    }
    if meta.in_type.as_deref() != Some(record.in_type.as_str())
        || meta.out_type.as_deref() != Some(record.out_type.as_str())
    {
        return Err(TypedAgentDispatchRefusal {
            code: "typed_workflow_signature_mismatch",
            message: format!(
                "current Loom node {node} frozen I/O does not match @{expected_type} revision {expected_rev}"
            ),
            install_job: None,
        });
    }
    let task = if meta.task.trim().is_empty() {
        meta.source_name.as_str()
    } else {
        meta.task.as_str()
    };
    let prompt = format!(
        "Daemon-selected Loom node {} in workflow {}. Execute only this scoped node task: {}. Record its outcome against the current graph obligation.",
        node, workflow.id, task
    );
    let dispatch = prepare_typed_dispatch(record, install_job, task, &prompt)?;
    Ok(Some(TypedWorkflowNodeDispatchPlan {
        workflow_id: workflow.id.clone(),
        node: node.clone(),
        dispatch,
    }))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
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
                current_cli: matches!(state, TypedAgentInstallState::Installing)
                    .then(|| "rg".into()),
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
        let ast =
            parse_pipe("typed-flow: Question -> Sources\nresearch @researcher \"find sources\"");
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
}
