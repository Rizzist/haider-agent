//! Thin typed client surface for the convergence-graph RPC family.
//!
//! Attachment ownership remains with the caller: graph mutations require the
//! worker generation from a control attachment, while `graph.status` is a
//! receipt-free view and needs no attachment.

use haider_protocol::graph::{GraphInspectSnapshot, GraphStatus as ConvergenceGraphStatus};
use haider_protocol::ids::{GraphId, SessionId};
use haider_rpc::{CommandId, RequestBody, ResponseBody};

use crate::{ClientError, RpcClient};

/// Successful durable coordinates returned by `graph.pin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPinResult {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
    pub pinned_seq: u64,
    pub opened_seq: u64,
    pub worker_generation: u64,
}

/// Successful durable coordinates returned by `graph.abandon`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAbandonResult {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub abandoned_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSwitchResult {
    pub session_id: SessionId,
    pub old_graph_id: GraphId,
    pub new_graph_id: GraphId,
    pub template: String,
    pub digest: String,
    pub superseded_seq: u64,
    pub pinned_seq: u64,
    pub opened_seq: u64,
    pub worker_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphInspectPage {
    pub snapshot: GraphInspectSnapshot,
    pub next_cursor: Option<String>,
}

/// A transport failure, daemon rejection, or mismatched response method.
#[derive(Debug)]
pub enum GraphClientError {
    Client(ClientError),
    Rpc {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
}

impl std::fmt::Display for GraphClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Rpc { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for GraphClientError {}

impl From<ClientError> for GraphClientError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

fn rpc_error(body: ResponseBody) -> Result<ResponseBody, GraphClientError> {
    match body {
        ResponseBody::Error {
            code,
            message,
            retryable,
            ..
        } => Err(GraphClientError::Rpc {
            code,
            message,
            retryable,
        }),
        body => Ok(body),
    }
}

/// Reads the daemon's current reduction. `None` means no graph has ever been
/// pinned for this session; terminal graphs remain queryable journal truth.
pub async fn graph_status(
    client: &RpcClient,
    session_id: SessionId,
) -> Result<Option<ConvergenceGraphStatus>, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphStatus { session_id })
            .await?,
    )? {
        ResponseBody::GraphStatus { status } => Ok(status),
        _ => Err(GraphClientError::Protocol(
            "graph.status response method mismatch",
        )),
    }
}

/// Reads one bounded, snapshot-bound page of graph telemetry and evidence
/// provenance. Pass `next_cursor` back verbatim for the following page.
pub async fn graph_inspect(
    client: &RpcClient,
    session_id: SessionId,
    cursor: Option<String>,
    limit: u32,
) -> Result<GraphInspectPage, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphInspect {
                session_id,
                cursor,
                limit,
            })
            .await?,
    )? {
        ResponseBody::GraphInspect {
            snapshot,
            next_cursor,
        } => Ok(GraphInspectPage {
            snapshot,
            next_cursor,
        }),
        _ => Err(GraphClientError::Protocol(
            "graph.inspect response method mismatch",
        )),
    }
}

/// Pins the built-in ship loop using a caller-owned control attachment.
pub async fn graph_pin(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
) -> Result<GraphPinResult, GraphClientError> {
    graph_pin_template(
        client,
        command_id,
        session_id,
        worker_generation,
        haider_protocol::graph::SHIP_LOOP_TEMPLATE.to_owned(),
    )
    .await
}

pub async fn graph_pin_template(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    template: String,
) -> Result<GraphPinResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphPin {
                command_id,
                session_id,
                worker_generation,
                template,
            })
            .await?,
    )? {
        ResponseBody::GraphPin {
            session_id,
            graph_id,
            template,
            digest,
            pinned_seq,
            opened_seq,
            worker_generation,
        } => Ok(GraphPinResult {
            session_id,
            graph_id,
            template,
            digest,
            pinned_seq,
            opened_seq,
            worker_generation,
        }),
        _ => Err(GraphClientError::Protocol(
            "graph.pin response method mismatch",
        )),
    }
}

pub async fn graph_switch(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    old_graph_id: GraphId,
    template: String,
) -> Result<GraphSwitchResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphSwitch {
                command_id,
                session_id,
                worker_generation,
                old_graph_id,
                template,
            })
            .await?,
    )? {
        ResponseBody::GraphSwitch {
            session_id,
            old_graph_id,
            new_graph_id,
            template,
            digest,
            superseded_seq,
            pinned_seq,
            opened_seq,
            worker_generation,
        } => Ok(GraphSwitchResult {
            session_id,
            old_graph_id,
            new_graph_id,
            template,
            digest,
            superseded_seq,
            pinned_seq,
            opened_seq,
            worker_generation,
        }),
        _ => Err(GraphClientError::Protocol(
            "graph.switch response method mismatch",
        )),
    }
}

/// Abandons the active graph using a caller-owned control attachment.
pub async fn graph_abandon(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    why: String,
) -> Result<GraphAbandonResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphAbandon {
                command_id,
                session_id,
                worker_generation,
                why,
            })
            .await?,
    )? {
        ResponseBody::GraphAbandon {
            session_id,
            graph_id,
            abandoned_seq,
            worker_generation,
        } => Ok(GraphAbandonResult {
            session_id,
            graph_id,
            abandoned_seq,
            worker_generation,
        }),
        _ => Err(GraphClientError::Protocol(
            "graph.abandon response method mismatch",
        )),
    }
}
