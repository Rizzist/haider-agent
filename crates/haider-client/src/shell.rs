//! Typed client seam for direct user shell commands.

use crate::{ClientError, RpcClient};
use haider_protocol::ids::{AgentId, BranchId, ItemId, RunId, SessionId};
use haider_rpc::{
    CancelStatus, CommandId, FEATURE_SHELL_EXEC_V1, FEATURE_TURN_CONTROL_V1,
    FEATURE_USER_COMMAND_V1, RequestBody, ResponseBody,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExecRequest {
    pub command_id: CommandId,
    pub session_id: SessionId,
    pub worker_generation: u64,
    pub branch_id: Option<BranchId>,
    pub agent_id: Option<AgentId>,
    pub command: String,
    pub cwd: Option<String>,
}

impl ShellExecRequest {
    #[must_use]
    pub fn request_body(&self) -> RequestBody {
        RequestBody::ShellExecScoped {
            command_id: self.command_id.clone(),
            session_id: self.session_id.clone(),
            worker_generation: self.worker_generation,
            branch_id: self.branch_id.clone(),
            agent_id: self.agent_id.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedShellExec {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub item_id: ItemId,
    pub accepted_seq: u64,
    pub worker_generation: u64,
}

impl AcceptedShellExec {
    #[must_use]
    pub fn cancel_request(&self, command_id: CommandId) -> RequestBody {
        RequestBody::TurnCancel {
            command_id,
            session_id: self.session_id.clone(),
            worker_generation: self.worker_generation,
            run_id: self.run_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledShellExec {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub status: CancelStatus,
    pub terminal_seq: Option<u64>,
}

#[derive(Debug)]
pub enum ShellExecError {
    Client(ClientError),
    Daemon {
        code: String,
        message: String,
        retryable: bool,
    },
    FeatureUnavailable {
        missing: Vec<String>,
    },
    /// An older `shell_exec_v1` daemon omitted the additive run coordinate.
    /// Generic clients can still recover it from the accepted item envelope;
    /// this typed helper refuses to pretend immediate cancellation is ready.
    MissingRunId,
    UnexpectedResponse(&'static str),
}

impl std::fmt::Display for ShellExecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "shell RPC failed: {error}"),
            Self::Daemon {
                code,
                message,
                retryable,
            } => write!(
                formatter,
                "shell RPC was rejected ({code}, retryable={retryable}): {message}"
            ),
            Self::FeatureUnavailable { missing } => write!(
                formatter,
                "daemon is missing required user-command features: {}",
                missing.join(", ")
            ),
            Self::MissingRunId => formatter.write_str(
                "shell RPC response omitted the run id required for immediate cancellation",
            ),
            Self::UnexpectedResponse(expected) => {
                write!(
                    formatter,
                    "shell RPC returned an unexpected response; expected {expected}"
                )
            }
        }
    }
}

impl std::error::Error for ShellExecError {}

impl From<ClientError> for ShellExecError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

pub async fn shell_exec(
    client: &RpcClient,
    request: &ShellExecRequest,
) -> Result<AcceptedShellExec, ShellExecError> {
    let missing = required_user_command_features()
        .into_iter()
        .filter(|feature| !client.welcome().features.contains(feature))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ShellExecError::FeatureUnavailable { missing });
    }
    accepted_from_response(request, client.request(request.request_body()).await?)
}

pub async fn cancel_shell_exec(
    client: &RpcClient,
    accepted: &AcceptedShellExec,
    command_id: CommandId,
) -> Result<CancelledShellExec, ShellExecError> {
    cancelled_from_response(
        accepted,
        client.request(accepted.cancel_request(command_id)).await?,
    )
}

#[must_use]
pub fn required_user_command_features() -> BTreeSet<String> {
    BTreeSet::from([
        FEATURE_SHELL_EXEC_V1.to_owned(),
        FEATURE_TURN_CONTROL_V1.to_owned(),
        FEATURE_USER_COMMAND_V1.to_owned(),
    ])
}

pub(crate) fn accepted_from_response(
    request: &ShellExecRequest,
    response: ResponseBody,
) -> Result<AcceptedShellExec, ShellExecError> {
    match response {
        ResponseBody::ShellExec {
            session_id,
            run_id: Some(run_id),
            item_id,
            accepted_seq,
            worker_generation,
        } if session_id == request.session_id && worker_generation == request.worker_generation => {
            Ok(AcceptedShellExec {
                session_id,
                run_id,
                item_id,
                accepted_seq,
                worker_generation,
            })
        }
        ResponseBody::ShellExec {
            run_id: Some(_), ..
        } => Err(ShellExecError::UnexpectedResponse(
            "shell.exec coordinates matching the request",
        )),
        ResponseBody::ShellExec { run_id: None, .. } => Err(ShellExecError::MissingRunId),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ShellExecError::Daemon {
            code,
            message,
            retryable,
        }),
        _ => Err(ShellExecError::UnexpectedResponse("shell.exec")),
    }
}

pub(crate) fn cancelled_from_response(
    accepted: &AcceptedShellExec,
    response: ResponseBody,
) -> Result<CancelledShellExec, ShellExecError> {
    match response {
        ResponseBody::TurnCancel {
            session_id,
            run_id,
            status,
            terminal_seq,
        } if session_id == accepted.session_id && run_id == accepted.run_id => {
            Ok(CancelledShellExec {
                session_id,
                run_id,
                status,
                terminal_seq,
            })
        }
        ResponseBody::TurnCancel { .. } => Err(ShellExecError::UnexpectedResponse(
            "turn.cancel coordinates matching the accepted shell run",
        )),
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(ShellExecError::Daemon {
            code,
            message,
            retryable,
        }),
        _ => Err(ShellExecError::UnexpectedResponse("turn.cancel")),
    }
}
