#![allow(clippy::expect_used)]

use haider_protocol::loom::LoomAgentType;
use haider_protocol::typed_agent::{
    TypedAgentContract, TypedAgentContractErrorCode, TypedAgentInstallJob, TypedAgentInstallState,
};

fn loom_type() -> LoomAgentType {
    LoomAgentType {
        id: "researcher".into(),
        name: "Research specialist".into(),
        job: "Gather primary evidence inside the declared scope.".into(),
        in_type: "Question".into(),
        out_type: "Evidence".into(),
        clis: vec!["rg".into(), "jq".into(), "rg".into()],
        apis: Vec::new(),
        skills: Vec::new(),
        scripts: Vec::new(),
        color: "#c2701c".into(),
        glyph: "▲".into(),
        rev: 3,
    }
}

#[test]
fn loom_type_derives_an_explicit_scoped_role_and_required_clis() {
    let source = loom_type();
    let contract = TypedAgentContract::from_loom_agent_type(&source).expect("valid contract");

    assert_eq!(contract.agent_type_id, "researcher");
    assert_eq!(contract.agent_type_rev, 3);
    assert_eq!(contract.agent_type_digest, source.digest());
    assert_eq!(contract.role.scope, "researcher");
    assert_eq!(contract.role.name, "Research specialist");
    assert_eq!(contract.role.instructions, source.job);
    assert_eq!(
        contract
            .required_clis
            .iter()
            .map(|required| required.program.as_str())
            .collect::<Vec<_>>(),
        ["rg", "jq"]
    );
}

#[test]
fn authored_shells_and_dispatchers_are_not_required_cli_contracts() {
    for unsafe_program in ["bash", "env", "relative/tool", "../tool", "tool;rm"] {
        let mut source = loom_type();
        source.clis = vec![unsafe_program.into()];
        let error = TypedAgentContract::from_loom_agent_type(&source)
            .expect_err("unsafe executable contract must fail");
        assert_eq!(error.code, TypedAgentContractErrorCode::InvalidRequiredCli);
    }
}

#[test]
fn install_job_progress_and_state_transitions_are_monotonic() {
    let contract = TypedAgentContract::from_loom_agent_type(&loom_type()).expect("contract");
    let queued = TypedAgentInstallJob::queued("install:researcher:3", &contract, 10)
        .expect("queued install");
    assert_eq!(queued.state, TypedAgentInstallState::Queued);
    assert_eq!((queued.progress.completed, queued.progress.total), (0, 2));

    let mut installing = queued.clone();
    installing.state = TypedAgentInstallState::Installing;
    installing.progress.current_cli = Some("rg".into());
    installing.updated_at_ms = 11;
    queued
        .validate_update(&installing)
        .expect("queued to installing");

    let mut verifying = installing.clone();
    verifying.state = TypedAgentInstallState::Verifying;
    verifying.progress.completed = 2;
    verifying.progress.current_cli = Some("jq".into());
    verifying.updated_at_ms = 12;
    installing
        .validate_update(&verifying)
        .expect("installing to verifying");

    let mut succeeded = verifying.clone();
    succeeded.state = TypedAgentInstallState::Succeeded;
    succeeded.progress.current_cli = None;
    succeeded.updated_at_ms = 13;
    verifying
        .validate_update(&succeeded)
        .expect("verifying to succeeded");
    assert!(succeeded.state.is_terminal());

    let transition_error = TypedAgentInstallState::Succeeded
        .validate_transition_to(TypedAgentInstallState::Installing)
        .expect_err("terminal state cannot reopen");
    assert_eq!(
        transition_error.code,
        TypedAgentContractErrorCode::IllegalInstallTransition
    );

    let error = succeeded
        .validate_update(&installing)
        .expect_err("terminal job cannot reopen");
    assert_eq!(
        error.code,
        TypedAgentContractErrorCode::InvalidInstallProgress
    );
}

#[test]
fn install_jobs_reject_inconsistent_or_unbounded_progress() {
    let contract = TypedAgentContract::from_loom_agent_type(&loom_type()).expect("contract");
    let mut job =
        TypedAgentInstallJob::queued("install:researcher:3", &contract, 10).expect("queued job");

    job.state = TypedAgentInstallState::Succeeded;
    assert_eq!(
        job.validate().expect_err("incomplete success").code,
        TypedAgentContractErrorCode::InvalidInstallProgress
    );

    job.state = TypedAgentInstallState::Failed;
    job.error = Some("x".repeat(513));
    assert_eq!(
        job.validate().expect_err("oversized error").code,
        TypedAgentContractErrorCode::InvalidInstallJob
    );
}
