//! One deliberately small conversion seam between L2's durable workflow
//! graph RPC and the transport-independent client projection.
//!
//! L2 owns validation and event reduction. This adapter retains its typed
//! state, calls that reducer for watch events, and derives only presentation
//! facts: the five UI statuses, input-presence lights, and evidence refs.

use std::collections::{BTreeMap, BTreeSet};

use haider_protocol::graph::{
    WorkflowEdgeKind, WorkflowGraphJournalEvent, WorkflowGraphWatchPage as RpcWatchPage,
    WorkflowNodePhase,
};

use crate::workflow_graph::{
    WorkflowEvidenceRef, WorkflowGraphChange, WorkflowGraphEdge, WorkflowGraphEdgeKind,
    WorkflowGraphState, WorkflowGraphWatchPage, WorkflowNodeProjection, WorkflowNodeRejection,
    WorkflowNodeState,
};

/// Refusal to narrow an L2 RPC state/page into the live-view projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowGraphRpcAdapterError {
    BaselineRequired,
    CursorMismatch { baseline: u64, requested_after: u64 },
    InvalidState(&'static str),
    InvalidWatchPage(&'static str),
    RebaselineRequired,
    Reduction(String),
}

impl std::fmt::Display for WorkflowGraphRpcAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BaselineRequired => {
                formatter.write_str("workflow graph watch arrived before its state baseline")
            }
            Self::CursorMismatch {
                baseline,
                requested_after,
            } => write!(
                formatter,
                "workflow graph RPC cursor mismatch: baseline {baseline}, watch requested {requested_after}"
            ),
            Self::InvalidState(message) => {
                write!(formatter, "invalid workflow graph RPC state: {message}")
            }
            Self::InvalidWatchPage(message) => {
                write!(
                    formatter,
                    "invalid workflow graph RPC watch page: {message}"
                )
            }
            Self::RebaselineRequired => formatter.write_str(
                "workflow graph watch changed graph identity; a fresh state baseline is required",
            ),
            Self::Reduction(message) => {
                write!(formatter, "workflow graph RPC reduction failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkflowGraphRpcAdapterError {}

/// Typed L2 state retained solely to reduce subsequent watch pages. The TUI
/// renders [`WorkflowGraphState`], never this engine-owned representation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowGraphRpcAdapter {
    state: Option<haider_protocol::graph::WorkflowGraphState>,
    /// Session-journal cursor fully scanned by watch pages. This may be
    /// ahead of `state.through_cursor` when an empty page crosses unrelated
    /// session facts; L2's reducer cursor advances only on graph events.
    applied_cursor: Option<u64>,
}

impl WorkflowGraphRpcAdapter {
    pub fn clear(&mut self) {
        self.state = None;
        self.applied_cursor = None;
    }

    /// Install a new L2 baseline and derive one complete view state.
    pub fn replace(
        &mut self,
        state: haider_protocol::graph::WorkflowGraphState,
    ) -> Result<WorkflowGraphState, WorkflowGraphRpcAdapterError> {
        let projected = project_state(&state)?;
        self.applied_cursor = Some(state.through_cursor);
        self.state = Some(state);
        Ok(projected)
    }

    /// Reduce and adapt one bounded L2 watch page atomically. A page naming
    /// another graph is not guessed across: the caller reissues
    /// `workflow.graph.state`, whose default selects the latest graph.
    pub fn apply_page(
        &mut self,
        applied_cursor: u64,
        page: RpcWatchPage,
    ) -> Result<WorkflowGraphWatchPage, WorkflowGraphRpcAdapterError> {
        let Some(source) = self.state.as_ref() else {
            return Err(WorkflowGraphRpcAdapterError::BaselineRequired);
        };
        validate_watch_metadata(&page, applied_cursor)?;
        let Some(retained_cursor) = self.applied_cursor else {
            return Err(WorkflowGraphRpcAdapterError::BaselineRequired);
        };
        if retained_cursor != applied_cursor {
            return Err(WorkflowGraphRpcAdapterError::CursorMismatch {
                baseline: retained_cursor,
                requested_after: applied_cursor,
            });
        }
        if page.requested_after_cursor < retained_cursor && page.next_cursor <= retained_cursor {
            return Ok(WorkflowGraphWatchPage {
                graph_id: source.graph_id.as_str().to_owned(),
                workflow_id: source.ast.workflow_id.clone(),
                workflow_digest: source.ast.workflow_digest.clone(),
                requested_after_cursor: page.requested_after_cursor,
                // This internal page represents an already-applied duplicate,
                // so it cannot legitimately ask the UI to drain a suffix.
                replay_through_cursor: page.next_cursor,
                next_cursor: page.next_cursor,
                changes: Vec::new(),
            });
        }
        if page.requested_after_cursor != applied_cursor {
            return Err(WorkflowGraphRpcAdapterError::CursorMismatch {
                baseline: applied_cursor,
                requested_after: page.requested_after_cursor,
            });
        }
        if source.through_cursor > retained_cursor {
            return Err(WorkflowGraphRpcAdapterError::InvalidState(
                "reducer cursor is ahead of the retained watch cursor",
            ));
        }
        let workflow_id = source.ast.workflow_id.clone();
        let workflow_digest = source.ast.workflow_digest.clone();
        let graph_id = source.graph_id.clone();
        let mut next = source.clone();
        let mut prior = projected_nodes(&next)?;
        let mut changes = Vec::new();
        let mut event_cursor = applied_cursor;
        for watched in page.events {
            if watched.cursor <= event_cursor || watched.cursor > page.replay_through_cursor {
                return Err(WorkflowGraphRpcAdapterError::InvalidWatchPage(
                    "event cursors do not strictly advance within the replay bound",
                ));
            }
            if event_graph_id(&watched.event) != &graph_id {
                return Err(WorkflowGraphRpcAdapterError::RebaselineRequired);
            }
            next.apply(watched.cursor, &watched.event)
                .map_err(|error| WorkflowGraphRpcAdapterError::Reduction(error.to_string()))?;
            let projected = projected_nodes(&next)?;
            for (node_id, node) in &projected {
                if prior.get(node_id) != Some(node) {
                    changes.push(WorkflowGraphChange {
                        cursor: watched.cursor,
                        node: node.clone(),
                    });
                }
            }
            prior = projected;
            event_cursor = watched.cursor;
        }
        let adapted = WorkflowGraphWatchPage {
            graph_id: graph_id.as_str().to_owned(),
            workflow_id,
            workflow_digest,
            requested_after_cursor: page.requested_after_cursor,
            replay_through_cursor: page.replay_through_cursor,
            next_cursor: page.next_cursor,
            changes,
        };
        self.state = Some(next);
        self.applied_cursor = Some(adapted.next_cursor);
        Ok(adapted)
    }
}

fn project_state(
    state: &haider_protocol::graph::WorkflowGraphState,
) -> Result<WorkflowGraphState, WorkflowGraphRpcAdapterError> {
    validate_state_shape(state)?;
    Ok(WorkflowGraphState {
        graph_id: state.graph_id.as_str().to_owned(),
        workflow_id: state.ast.workflow_id.clone(),
        workflow_digest: state.ast.workflow_digest.clone(),
        cursor: state.through_cursor,
        nodes: state
            .ast
            .nodes
            .iter()
            .map(|spec| project_node(state, &spec.node))
            .collect::<Result<Vec<_>, _>>()?,
        edges: state
            .ast
            .edges
            .iter()
            .map(|edge| WorkflowGraphEdge {
                kind: match edge.kind {
                    WorkflowEdgeKind::GraphInput => WorkflowGraphEdgeKind::GraphInput,
                    WorkflowEdgeKind::Forward => WorkflowGraphEdgeKind::Forward,
                    WorkflowEdgeKind::Back => WorkflowGraphEdgeKind::Back,
                },
                from: edge.from.as_ref().map(|node| node.as_str().to_owned()),
                to: edge.to.as_str().to_owned(),
            })
            .collect(),
    })
}

fn projected_nodes(
    state: &haider_protocol::graph::WorkflowGraphState,
) -> Result<BTreeMap<String, WorkflowNodeProjection>, WorkflowGraphRpcAdapterError> {
    Ok(project_state(state)?
        .nodes
        .into_iter()
        .map(|node| (node.node_id.clone(), node))
        .collect())
}

fn project_node(
    state: &haider_protocol::graph::WorkflowGraphState,
    node_id: &haider_protocol::graph::GraphNodeName,
) -> Result<WorkflowNodeProjection, WorkflowGraphRpcAdapterError> {
    let spec = state
        .ast
        .nodes
        .iter()
        .find(|candidate| &candidate.node == node_id)
        .ok_or(WorkflowGraphRpcAdapterError::InvalidState(
            "AST node disappeared during projection",
        ))?;
    let node = state
        .node(node_id)
        .ok_or(WorkflowGraphRpcAdapterError::InvalidState(
            "AST node has no runtime state",
        ))?;
    let initial_inputs_present = spec
        .join
        .initial_all
        .iter()
        .map(|edge_id| input_is_present(state, node_id, *edge_id))
        .collect::<Vec<_>>();
    let reactivation_inputs_present = spec
        .join
        .reactivate_any
        .iter()
        .map(|edge_id| input_is_present(state, node_id, *edge_id))
        .collect::<Vec<_>>();
    let ready = initial_inputs_present.iter().all(|present| *present)
        || reactivation_inputs_present.iter().any(|present| *present);
    let inputs_present = initial_inputs_present
        .into_iter()
        .chain(reactivation_inputs_present)
        .collect();
    let status = match node.phase {
        WorkflowNodePhase::Waiting if ready => WorkflowNodeState::Ready,
        WorkflowNodePhase::Waiting => WorkflowNodeState::Waiting,
        WorkflowNodePhase::Activated => WorkflowNodeState::Active,
        WorkflowNodePhase::Completed => WorkflowNodeState::Complete,
        WorkflowNodePhase::Rejected => WorkflowNodeState::Rejected,
    };
    let mut evidence = BTreeMap::new();
    if node.phase == WorkflowNodePhase::Rejected {
        if let Some(reference) = node
            .rejection
            .as_ref()
            .and_then(|rejection| rejection.evidence.as_ref())
        {
            evidence.insert(
                reference.artifact.as_str().to_owned(),
                reference.artifact.clone(),
            );
        }
    } else {
        evidence.extend(node.inputs.iter().map(|input| {
            (
                input.evidence.artifact.as_str().to_owned(),
                input.evidence.artifact.clone(),
            )
        }));
        evidence.extend(
            node.outputs
                .iter()
                .map(|output| (output.artifact.as_str().to_owned(), output.artifact.clone())),
        );
    }
    let rejection = node
        .rejection
        .as_ref()
        .map(|rejection| WorkflowNodeRejection {
            code: rejection.code,
            message: rejection.message.clone(),
            cursor: node.updated_cursor,
            evidence: rejection
                .evidence
                .as_ref()
                .map(|reference| WorkflowEvidenceRef::new(reference.artifact.clone())),
        });
    Ok(WorkflowNodeProjection {
        node_id: node_id.as_str().to_owned(),
        status,
        inputs_present,
        evidence_refs: evidence
            .into_values()
            .map(WorkflowEvidenceRef::new)
            .collect(),
        rejection,
    })
}

fn validate_state_shape(
    state: &haider_protocol::graph::WorkflowGraphState,
) -> Result<(), WorkflowGraphRpcAdapterError> {
    if state.graph_id.as_str().is_empty()
        || state.ast.workflow_id.is_empty()
        || state.ast.workflow_digest.is_empty()
    {
        return Err(WorkflowGraphRpcAdapterError::InvalidState(
            "graph or workflow identity is empty",
        ));
    }
    let ast_nodes = state
        .ast
        .nodes
        .iter()
        .map(|node| node.node.as_str())
        .collect::<BTreeSet<_>>();
    let runtime_nodes = state
        .nodes
        .iter()
        .map(|node| node.node.as_str())
        .collect::<BTreeSet<_>>();
    if ast_nodes.len() != state.ast.nodes.len()
        || runtime_nodes.len() != state.nodes.len()
        || ast_nodes != runtime_nodes
    {
        return Err(WorkflowGraphRpcAdapterError::InvalidState(
            "AST and runtime node sets differ",
        ));
    }
    Ok(())
}

fn validate_watch_metadata(
    page: &RpcWatchPage,
    applied_cursor: u64,
) -> Result<(), WorkflowGraphRpcAdapterError> {
    if page.replay_through_cursor < page.requested_after_cursor
        || page.next_cursor < page.requested_after_cursor
        || page.next_cursor > page.replay_through_cursor
    {
        return Err(WorkflowGraphRpcAdapterError::InvalidWatchPage(
            "cursor bounds are inconsistent",
        ));
    }
    let expected_next = page
        .events
        .last()
        .map_or(page.replay_through_cursor, |event| event.cursor);
    if page.next_cursor != expected_next {
        return Err(WorkflowGraphRpcAdapterError::InvalidWatchPage(
            "next cursor does not match the page tail",
        ));
    }
    if page.requested_after_cursor > applied_cursor {
        return Err(WorkflowGraphRpcAdapterError::CursorMismatch {
            baseline: applied_cursor,
            requested_after: page.requested_after_cursor,
        });
    }
    Ok(())
}

fn input_is_present(
    state: &haider_protocol::graph::WorkflowGraphState,
    node_id: &haider_protocol::graph::GraphNodeName,
    edge_id: u32,
) -> bool {
    if state
        .node(node_id)
        .is_some_and(|node| node.inputs.iter().any(|input| input.edge_id == edge_id))
    {
        return true;
    }
    let Some(edge) = state
        .ast
        .edges
        .iter()
        .find(|edge| edge.id == edge_id && &edge.to == node_id)
    else {
        return false;
    };
    match edge.kind {
        WorkflowEdgeKind::GraphInput => {
            edge.from.is_none()
                && state
                    .seed
                    .as_ref()
                    .is_some_and(|seed| seed.evidence_type == edge.evidence_type)
        }
        WorkflowEdgeKind::Forward => edge.from.as_ref().is_some_and(|source| {
            state.node(source).is_some_and(|node| {
                node.phase == WorkflowNodePhase::Completed
                    && node
                        .outputs
                        .iter()
                        .any(|output| output.evidence_type == edge.evidence_type)
            })
        }),
        WorkflowEdgeKind::Back => edge.from.as_ref().is_some_and(|source| {
            state
                .node(source)
                .and_then(|node| node.rejection.as_ref())
                .and_then(|rejection| rejection.evidence.as_ref())
                .is_some_and(|evidence| evidence.evidence_type == edge.evidence_type)
        }),
    }
}

fn event_graph_id(event: &WorkflowGraphJournalEvent) -> &haider_protocol::ids::GraphId {
    match event {
        WorkflowGraphJournalEvent::WorkflowGraphStarted(started) => &started.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeActivated(activated) => &activated.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed) => &completed.graph_id,
        WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected) => &rejected.graph_id,
    }
}
