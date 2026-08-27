//! Feature-negotiated typed client helpers for durable workspace checkpoints.

use crate::{ClientError, RpcClient};
use haider_protocol::checkpoint::{
    CheckpointCursor, CheckpointListPage, CheckpointMutationReceipt,
};
use haider_protocol::ids::{BranchId, RunId, SessionId};
use haider_rpc::{CommandId, ErrorData, FEATURE_CHECKPOINT_V1, RequestBody, ResponseBody};

#[derive(Debug)]
pub enum CheckpointClientError {
    Client(ClientError),
    MissingFeature(&'static str),
    Daemon {
        code: String,
        message: String,
        retryable: bool,
        data: Option<ErrorData>,
    },
    UnexpectedResponse(&'static str),
}

impl std::fmt::Display for CheckpointClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "checkpoint RPC failed: {error}"),
            Self::MissingFeature(feature) => {
                write!(
                    formatter,
                    "daemon does not advertise required feature `{feature}`"
                )
            }
            Self::Daemon {
                code,
                message,
                retryable,
                ..
            } => write!(
                formatter,
                "checkpoint RPC was rejected ({code}, retryable={retryable}): {message}"
            ),
            Self::UnexpectedResponse(expected) => write!(
                formatter,
                "checkpoint RPC returned an unexpected response; expected {expected}"
            ),
        }
    }
}

impl std::error::Error for CheckpointClientError {}

impl From<ClientError> for CheckpointClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

fn require_checkpoint(client: &RpcClient) -> Result<(), CheckpointClientError> {
    if client.welcome().features.contains(FEATURE_CHECKPOINT_V1) {
        Ok(())
    } else {
        Err(CheckpointClientError::MissingFeature(FEATURE_CHECKPOINT_V1))
    }
}

fn response_error(response: ResponseBody) -> Result<ResponseBody, CheckpointClientError> {
    match response {
        ResponseBody::Error {
            code,
            message,
            retryable,
            data,
        } => Err(CheckpointClientError::Daemon {
            code,
            message,
            retryable,
            data,
        }),
        response => Ok(response),
    }
}

pub async fn checkpoints(
    client: &RpcClient,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    cursor: Option<CheckpointCursor>,
    limit: u16,
) -> Result<CheckpointListPage, CheckpointClientError> {
    require_checkpoint(client)?;
    match response_error(
        client
            .request(RequestBody::CheckpointList {
                session_id,
                branch_id,
                cursor,
                limit,
            })
            .await?,
    )? {
        ResponseBody::CheckpointList { page } => Ok(page),
        _ => Err(CheckpointClientError::UnexpectedResponse("checkpoint.list")),
    }
}

pub async fn undo(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    worker_generation: u64,
    target: impl Into<String>,
) -> Result<CheckpointMutationReceipt, CheckpointClientError> {
    checkpoint_mutation(
        client,
        RequestBody::CheckpointUndo {
            command_id,
            session_id,
            branch_id,
            worker_generation,
            target: target.into(),
        },
        "checkpoint.undo",
    )
    .await
}

pub async fn redo(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    worker_generation: u64,
    target: impl Into<String>,
) -> Result<CheckpointMutationReceipt, CheckpointClientError> {
    checkpoint_mutation(
        client,
        RequestBody::CheckpointRedo {
            command_id,
            session_id,
            branch_id,
            worker_generation,
            target: target.into(),
        },
        "checkpoint.redo",
    )
    .await
}

pub async fn rollback_turn(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    branch_id: Option<BranchId>,
    worker_generation: u64,
    run_id: RunId,
) -> Result<CheckpointMutationReceipt, CheckpointClientError> {
    checkpoint_mutation(
        client,
        RequestBody::CheckpointRollbackTurn {
            command_id,
            session_id,
            branch_id,
            worker_generation,
            run_id,
        },
        "checkpoint.rollback_turn",
    )
    .await
}

async fn checkpoint_mutation(
    client: &RpcClient,
    request: RequestBody,
    expected: &'static str,
) -> Result<CheckpointMutationReceipt, CheckpointClientError> {
    require_checkpoint(client)?;
    match response_error(client.request(request).await?)? {
        ResponseBody::CheckpointUndo { receipt } if expected == "checkpoint.undo" => Ok(receipt),
        ResponseBody::CheckpointRedo { receipt } if expected == "checkpoint.redo" => Ok(receipt),
        ResponseBody::CheckpointRollbackTurn { receipt }
            if expected == "checkpoint.rollback_turn" =>
        {
            Ok(receipt)
        }
        _ => Err(CheckpointClientError::UnexpectedResponse(expected)),
    }
}
