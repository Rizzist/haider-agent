//! Thin typed client surface for the convergence-graph RPC family.
//!
//! Attachment ownership remains with the caller: graph mutations require the
//! worker generation from a control attachment, while `graph.status` is a
//! receipt-free view and needs no attachment.

use std::collections::BTreeSet;

use haider_protocol::graph::{GraphInspectSnapshot, GraphStatus as ConvergenceGraphStatus};
use haider_protocol::ids::{GraphId, GraphRunSetId, ItemId, SessionId};
use haider_rpc::{
    CommandId, ERROR_CODE_REVISION_CONFLICT, ErrorData, FEATURE_WORKFLOW_INSTANCE_V1, RequestBody,
    ResponseBody, TodoGraphOpenedWire, WorkflowInstanceV1,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRunSetOpenResult {
    pub session_id: SessionId,
    pub run_set_id: GraphRunSetId,
    pub root_graph_id: GraphId,
    pub plan_item_id: ItemId,
    pub plan_event_seq: u64,
    pub template: String,
    pub digest: String,
    pub run_set_opened_seq: u64,
    pub through_seq: u64,
    pub children: Vec<TodoGraphOpenedWire>,
    pub worker_generation: u64,
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
    WorkflowRevisionConflict {
        expected_digest: String,
        current_digest: String,
        current_revision: u32,
        message: String,
        retryable: bool,
    },
    MissingFeature(&'static str),
    Protocol(&'static str),
}

impl std::fmt::Display for GraphClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::Rpc { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::WorkflowRevisionConflict {
                current_digest,
                current_revision,
                message,
                ..
            } => write!(
                formatter,
                "{message} (current workflow revision {current_revision}, digest {current_digest})"
            ),
            Self::MissingFeature(feature) => {
                write!(
                    formatter,
                    "daemon does not advertise required feature `{feature}`"
                )
            }
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
            data:
                Some(ErrorData::WorkflowRevisionConflict {
                    expected_digest,
                    current_digest,
                    current_revision,
                }),
        } if code == ERROR_CODE_REVISION_CONFLICT => {
            Err(GraphClientError::WorkflowRevisionConflict {
                expected_digest,
                current_digest,
                current_revision,
                message,
                retryable,
            })
        }
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

fn negotiated_expected_digest(
    features: &BTreeSet<String>,
    expected_digest: String,
) -> Option<String> {
    features
        .contains(FEATURE_WORKFLOW_INSTANCE_V1)
        .then_some(expected_digest)
}

/// Reads either the daemon's current instance for `workflow_id` or the exact
/// retained revision named by a pinned graph's template digest.
pub async fn workflow_instance(
    client: &RpcClient,
    workflow_id: String,
    template_digest: Option<String>,
) -> Result<Option<WorkflowInstanceV1>, GraphClientError> {
    if !client
        .welcome()
        .features
        .contains(FEATURE_WORKFLOW_INSTANCE_V1)
    {
        return Err(GraphClientError::MissingFeature(
            FEATURE_WORKFLOW_INSTANCE_V1,
        ));
    }
    match rpc_error(
        client
            .request(RequestBody::WorkflowInstance {
                workflow_id,
                template_digest,
            })
            .await?,
    )? {
        ResponseBody::WorkflowInstance { instance } => Ok(instance),
        _ => Err(GraphClientError::Protocol(
            "workflow.instance response method mismatch",
        )),
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
    graph_pin_template_with_fence(
        client,
        command_id,
        session_id,
        worker_generation,
        template,
        None,
    )
    .await
}

/// Pins with a digest fence when `workflow_instance_v1` is negotiated. An
/// older daemon receives the legacy request with no fabricated digest.
pub async fn graph_pin_template_fenced(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    template: String,
    expected_digest: String,
) -> Result<GraphPinResult, GraphClientError> {
    let expected_digest = negotiated_expected_digest(&client.welcome().features, expected_digest);
    graph_pin_template_with_fence(
        client,
        command_id,
        session_id,
        worker_generation,
        template,
        expected_digest,
    )
    .await
}

async fn graph_pin_template_with_fence(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    template: String,
    expected_digest: Option<String>,
) -> Result<GraphPinResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphPin {
                command_id,
                session_id,
                worker_generation,
                template,
                expected_digest,
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

/// Opens one independently reduced child graph per todo in an exact durable
/// Plan fact.
pub async fn graph_run_set_open(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    plan_item_id: ItemId,
    plan_event_seq: u64,
) -> Result<GraphRunSetOpenResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphRunSetOpen {
                command_id,
                session_id,
                worker_generation,
                plan_item_id,
                plan_event_seq,
            })
            .await?,
    )? {
        ResponseBody::GraphRunSetOpen {
            session_id,
            run_set_id,
            root_graph_id,
            plan_item_id,
            plan_event_seq,
            template,
            digest,
            run_set_opened_seq,
            through_seq,
            children,
            worker_generation,
        } => Ok(GraphRunSetOpenResult {
            session_id,
            run_set_id,
            root_graph_id,
            plan_item_id,
            plan_event_seq,
            template,
            digest,
            run_set_opened_seq,
            through_seq,
            children,
            worker_generation,
        }),
        _ => Err(GraphClientError::Protocol(
            "graph.run_set.open response method mismatch",
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
    graph_switch_with_fence(
        client,
        command_id,
        session_id,
        worker_generation,
        old_graph_id,
        template,
        None,
    )
    .await
}

/// Switches with a digest fence when supported, otherwise sends the exact
/// legacy unfenced request.
pub async fn graph_switch_fenced(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    old_graph_id: GraphId,
    template: String,
    expected_digest: String,
) -> Result<GraphSwitchResult, GraphClientError> {
    let expected_digest = negotiated_expected_digest(&client.welcome().features, expected_digest);
    graph_switch_with_fence(
        client,
        command_id,
        session_id,
        worker_generation,
        old_graph_id,
        template,
        expected_digest,
    )
    .await
}

async fn graph_switch_with_fence(
    client: &RpcClient,
    command_id: CommandId,
    session_id: SessionId,
    worker_generation: u64,
    old_graph_id: GraphId,
    template: String,
    expected_digest: Option<String>,
) -> Result<GraphSwitchResult, GraphClientError> {
    match rpc_error(
        client
            .request(RequestBody::GraphSwitch {
                command_id,
                session_id,
                worker_generation,
                old_graph_id,
                template,
                expected_digest,
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn absent_workflow_instance_feature_omits_fence_instead_of_fabricating_one() {
        assert_eq!(
            negotiated_expected_digest(&BTreeSet::new(), "observed-digest".into()),
            None
        );
        assert_eq!(
            negotiated_expected_digest(
                &BTreeSet::from([FEATURE_WORKFLOW_INSTANCE_V1.to_owned()]),
                "observed-digest".into(),
            ),
            Some("observed-digest".into())
        );
    }
}
