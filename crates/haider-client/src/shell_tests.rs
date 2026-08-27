#![allow(clippy::expect_used)]

use super::shell::{
    AcceptedShellExec, ShellExecError, ShellExecRequest, accepted_from_response,
    cancelled_from_response, required_user_command_features,
};
use haider_protocol::ids::{AgentId, BranchId, ItemId, RunId, SessionId};
use haider_rpc::{
    CancelStatus, CommandId, FEATURE_SHELL_EXEC_V1, FEATURE_TURN_CONTROL_V1,
    FEATURE_USER_COMMAND_V1, RequestBody, ResponseBody,
};

#[test]
fn typed_shell_request_and_cancel_preserve_exact_coordinates() {
    let request = ShellExecRequest {
        command_id: CommandId::new("shell-command"),
        session_id: SessionId::new("session"),
        worker_generation: 9,
        branch_id: Some(BranchId::new("branch")),
        agent_id: Some(AgentId::new("agent")),
        command: "printf ' exact bytes '".into(),
        cwd: Some("nested".into()),
    };
    assert!(matches!(
        request.request_body(),
        RequestBody::ShellExecScoped {
            command_id,
            session_id,
            worker_generation: 9,
            branch_id: Some(branch_id),
            agent_id: Some(agent_id),
            command,
            cwd: Some(cwd),
        } if command_id.as_str() == "shell-command"
            && session_id.as_str() == "session"
            && branch_id.as_str() == "branch"
            && agent_id.as_str() == "agent"
            && command == "printf ' exact bytes '"
            && cwd == "nested"
    ));

    let accepted = AcceptedShellExec {
        session_id: request.session_id,
        run_id: RunId::new("shell-run"),
        item_id: ItemId::new("shell-item"),
        accepted_seq: 12,
        worker_generation: 9,
    };
    assert!(matches!(
        accepted.cancel_request(CommandId::new("cancel-command")),
        RequestBody::TurnCancel {
            command_id,
            session_id,
            worker_generation: 9,
            run_id,
        } if command_id.as_str() == "cancel-command"
            && session_id.as_str() == "session"
            && run_id.as_str() == "shell-run"
    ));
}

#[test]
fn typed_shell_response_requires_cancel_coordinate_and_preserves_daemon_errors() {
    let request = ShellExecRequest {
        command_id: CommandId::new("command"),
        session_id: SessionId::new("session"),
        worker_generation: 3,
        branch_id: None,
        agent_id: None,
        command: "printf ok".into(),
        cwd: None,
    };
    let Ok(accepted) = accepted_from_response(
        &request,
        ResponseBody::ShellExec {
            session_id: SessionId::new("session"),
            run_id: Some(RunId::new("run")),
            item_id: ItemId::new("item"),
            accepted_seq: 7,
            worker_generation: 3,
        },
    ) else {
        panic!("accepted response must decode");
    };
    assert_eq!(accepted.run_id.as_str(), "run");

    assert!(matches!(
        accepted_from_response(
            &request,
            ResponseBody::ShellExec {
                session_id: SessionId::new("session"),
                run_id: None,
                item_id: ItemId::new("item"),
                accepted_seq: 7,
                worker_generation: 3,
            }
        ),
        Err(ShellExecError::MissingRunId)
    ));

    assert!(matches!(
        accepted_from_response(
            &request,
            ResponseBody::ShellExec {
                session_id: SessionId::new("other-session"),
                run_id: Some(RunId::new("run")),
                item_id: ItemId::new("item"),
                accepted_seq: 7,
                worker_generation: 3,
            }
        ),
        Err(ShellExecError::UnexpectedResponse(_))
    ));

    assert!(matches!(
        accepted_from_response(&request, ResponseBody::Error {
            code: "busy".into(),
            message: "session busy".into(),
            retryable: true,
            data: None,
        }),
        Err(ShellExecError::Daemon { code, retryable: true, .. }) if code == "busy"
    ));

    let Ok(cancelled) = cancelled_from_response(
        &accepted,
        ResponseBody::TurnCancel {
            session_id: SessionId::new("session"),
            run_id: RunId::new("run"),
            status: CancelStatus::Accepted,
            terminal_seq: None,
        },
    ) else {
        panic!("cancel response must decode");
    };
    assert_eq!(cancelled.status, CancelStatus::Accepted);
    assert!(matches!(
        cancelled_from_response(
            &accepted,
            ResponseBody::TurnCancel {
                session_id: SessionId::new("session"),
                run_id: RunId::new("wrong-run"),
                status: CancelStatus::Accepted,
                terminal_seq: None,
            }
        ),
        Err(ShellExecError::UnexpectedResponse(_))
    ));
    assert_eq!(
        required_user_command_features(),
        std::collections::BTreeSet::from([
            FEATURE_SHELL_EXEC_V1.to_owned(),
            FEATURE_TURN_CONTROL_V1.to_owned(),
            FEATURE_USER_COMMAND_V1.to_owned(),
        ])
    );
}
