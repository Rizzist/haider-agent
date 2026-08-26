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
#[path = "typed_agent_executor_tests.rs"]
mod typed_agent_executor_tests;
