//! Client-side projection for the reconnectable `workflow.graph.watch`
//! stream.
//!
//! The engine and wire protocol live in the daemon/RPC layer. This module is
//! deliberately the thin consumer boundary: L2 snapshots become
//! [`WorkflowGraphState`] and bounded watch pages become
//! [`WorkflowGraphWatchPage`]. Everything after that conversion is
//! transport-independent and cursor exact.

use std::collections::{BTreeMap, BTreeSet};

use haider_protocol::graph::WorkflowNodeRejectCode;
use haider_protocol::ids::ArtifactRef;

/// The five runtime states published by `workflow.graph.state` and
/// `workflow.graph.watch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowNodeState {
    Waiting,
    Ready,
    Active,
    Complete,
    Rejected,
}

impl WorkflowNodeState {
    /// Whether this state prevents a node from doing any more work.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Rejected)
    }
}

/// One opaque evidence coordinate attached to a runtime node. The client
/// never parses or fetches this value; the workflow detail renders the
/// coordinate verbatim for inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEvidenceRef(ArtifactRef);

impl WorkflowEvidenceRef {
    #[must_use]
    pub const fn new(reference: ArtifactRef) -> Self {
        Self(reference)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[must_use]
    pub const fn artifact(&self) -> &ArtifactRef {
        &self.0
    }
}

/// Typed detail retained for every rejected node, even when L2 publishes no
/// optional evidence artifact. The journal cursor and reason remain
/// inspectable in that valid case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowNodeRejection {
    pub code: WorkflowNodeRejectCode,
    pub message: String,
    pub cursor: u64,
    pub evidence: Option<WorkflowEvidenceRef>,
}

impl WorkflowNodeRejection {
    #[must_use]
    pub const fn code_label(&self) -> &'static str {
        match self.code {
            WorkflowNodeRejectCode::EvidenceRejected => "evidence rejected",
            WorkflowNodeRejectCode::TypedInputMissing => "typed input missing",
            WorkflowNodeRejectCode::IterationGuard => "iteration guard",
            WorkflowNodeRejectCode::ConvergenceRejected => "convergence rejected",
            WorkflowNodeRejectCode::Abandoned => "abandoned",
            WorkflowNodeRejectCode::Superseded => "superseded",
            WorkflowNodeRejectCode::InvariantViolation => "invariant violation",
        }
    }
}

/// Minimal immutable topology copied from L2's frozen activation AST. The
/// client does not interpret execution; it retains enough typed structure to
/// draw the graph that actually ran instead of a later catalog revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkflowGraphEdgeKind {
    GraphInput,
    Forward,
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGraphEdge {
    pub kind: WorkflowGraphEdgeKind,
    pub from: Option<String>,
    pub to: String,
}

/// The complete runtime projection for one workflow node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowNodeProjection {
    pub node_id: String,
    pub status: WorkflowNodeState,
    /// One flag per declared typed input: initial-all inputs in declared
    /// order, followed by back-any reactivation inputs in declared order.
    pub inputs_present: Vec<bool>,
    /// Opaque daemon-owned evidence coordinates. Rejected nodes expose these
    /// through [`WorkflowGraphProjection::rejection_evidence`].
    pub evidence_refs: Vec<WorkflowEvidenceRef>,
    /// The typed rejection, including valid evidence-less rejections.
    pub rejection: Option<WorkflowNodeRejection>,
}

impl WorkflowNodeProjection {
    #[must_use]
    pub fn present_input_count(&self) -> usize {
        self.inputs_present
            .iter()
            .filter(|present| **present)
            .count()
    }

    #[must_use]
    pub fn all_inputs_present(&self) -> bool {
        self.inputs_present.iter().all(|present| *present)
    }
}

/// Thin adapter target for one `workflow.graph.state` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGraphState {
    pub graph_id: String,
    pub workflow_id: String,
    pub workflow_digest: String,
    pub cursor: u64,
    pub nodes: Vec<WorkflowNodeProjection>,
    pub edges: Vec<WorkflowGraphEdge>,
}

/// Thin adapter target for one `workflow.graph.watch` delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGraphChange {
    pub cursor: u64,
    pub node: WorkflowNodeProjection,
}

/// Thin adapter target for one bounded `workflow.graph.watch` page. Workflow
/// events can be sparse in the owning session journal, so continuity is
/// established by the page cursors rather than by requiring adjacent event
/// cursor values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowGraphWatchPage {
    pub graph_id: String,
    pub workflow_id: String,
    pub workflow_digest: String,
    pub requested_after_cursor: u64,
    pub replay_through_cursor: u64,
    pub next_cursor: u64,
    pub changes: Vec<WorkflowGraphChange>,
}

/// A malformed snapshot, a stream discontinuity, or a mismatched workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowGraphProjectionError {
    DuplicateNode { node_id: String },
    EmptyNodeId,
    GraphChanged { expected: String, observed: String },
    WorkflowChanged { expected: String, observed: String },
    DigestChanged { expected: String, observed: String },
    CursorMismatch { applied: u64, requested_after: u64 },
    InvalidWatchPage(&'static str),
    InvalidTopology(&'static str),
    UnknownNode { node_id: String },
}

impl std::fmt::Display for WorkflowGraphProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateNode { node_id } => {
                write!(
                    formatter,
                    "workflow graph contains duplicate node `{node_id}`"
                )
            }
            Self::EmptyNodeId => formatter.write_str("workflow graph contains an empty node id"),
            Self::GraphChanged { expected, observed } => write!(
                formatter,
                "workflow graph stream changed graph id from `{expected}` to `{observed}`"
            ),
            Self::WorkflowChanged { expected, observed } => write!(
                formatter,
                "workflow graph stream changed identity from `{expected}` to `{observed}`"
            ),
            Self::DigestChanged { expected, observed } => write!(
                formatter,
                "workflow graph stream changed digest from `{expected}` to `{observed}`"
            ),
            Self::CursorMismatch {
                applied,
                requested_after,
            } => write!(
                formatter,
                "workflow graph watch requested cursor {requested_after}, applied cursor is {applied}"
            ),
            Self::InvalidWatchPage(message) => {
                write!(formatter, "invalid workflow graph watch page: {message}")
            }
            Self::InvalidTopology(message) => {
                write!(formatter, "invalid workflow graph topology: {message}")
            }
            Self::UnknownNode { node_id } => {
                write!(
                    formatter,
                    "workflow graph update names unknown node `{node_id}`"
                )
            }
        }
    }
}

impl std::error::Error for WorkflowGraphProjectionError {}

/// Cursor-exact client reduction of a workflow graph watch stream.
///
/// The cursor advances only after every node change in a bounded page is
/// admitted. Duplicate pages behind the applied cursor are ignored; a request
/// discontinuity is typed so the caller can reconnect from [`Self::cursor`]
/// without applying any suffix after the hole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowGraphProjection {
    graph_id: Option<String>,
    workflow_id: Option<String>,
    workflow_digest: Option<String>,
    cursor: Option<u64>,
    order: Vec<String>,
    nodes: BTreeMap<String, WorkflowNodeProjection>,
    edges: Vec<WorkflowGraphEdge>,
}

impl WorkflowGraphProjection {
    #[must_use]
    pub fn graph_id(&self) -> Option<&str> {
        self.graph_id.as_deref()
    }

    #[must_use]
    pub fn workflow_id(&self) -> Option<&str> {
        self.workflow_id.as_deref()
    }

    #[must_use]
    pub fn workflow_digest(&self) -> Option<&str> {
        self.workflow_digest.as_deref()
    }

    /// Greatest watch cursor fully applied by this projection.
    #[must_use]
    pub const fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Clear the session's projection when `workflow.graph.state` reports
    /// that no activation graph exists.
    pub fn clear(&mut self) {
        self.graph_id = None;
        self.workflow_id = None;
        self.workflow_digest = None;
        self.cursor = None;
        self.order.clear();
        self.nodes.clear();
        self.edges.clear();
    }

    #[must_use]
    pub fn node(&self, node_id: &str) -> Option<&WorkflowNodeProjection> {
        self.nodes.get(node_id)
    }

    /// Nodes in the daemon snapshot's stable order.
    pub fn nodes(&self) -> impl Iterator<Item = &WorkflowNodeProjection> {
        self.order
            .iter()
            .filter_map(|node_id| self.nodes.get(node_id))
    }

    #[must_use]
    pub fn edges(&self) -> &[WorkflowGraphEdge] {
        &self.edges
    }

    /// Replace the projection with one complete daemon snapshot. Validation
    /// finishes before any live state is changed, so a malformed reconnect
    /// baseline cannot partially erase the last good view.
    pub fn replace(
        &mut self,
        state: WorkflowGraphState,
    ) -> Result<(), WorkflowGraphProjectionError> {
        if state.graph_id.is_empty() {
            return Err(WorkflowGraphProjectionError::InvalidTopology(
                "graph id is empty",
            ));
        }
        if state.workflow_id.is_empty() || state.workflow_digest.is_empty() {
            return Err(WorkflowGraphProjectionError::InvalidTopology(
                "workflow identity is empty",
            ));
        }
        if state.nodes.is_empty() {
            return Err(WorkflowGraphProjectionError::InvalidTopology(
                "node set is empty",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut order = Vec::with_capacity(state.nodes.len());
        let mut nodes = BTreeMap::new();
        for node in state.nodes {
            if node.node_id.is_empty() {
                return Err(WorkflowGraphProjectionError::EmptyNodeId);
            }
            if !ids.insert(node.node_id.clone()) {
                return Err(WorkflowGraphProjectionError::DuplicateNode {
                    node_id: node.node_id,
                });
            }
            order.push(node.node_id.clone());
            nodes.insert(node.node_id.clone(), node);
        }
        validate_edges(&state.edges, &ids)?;
        self.graph_id = Some(state.graph_id);
        self.workflow_id = Some(state.workflow_id);
        self.workflow_digest = Some(state.workflow_digest);
        self.cursor = Some(state.cursor);
        self.order = order;
        self.nodes = nodes;
        self.edges = state.edges;
        Ok(())
    }

    /// Apply one complete watch page atomically. The page must start at this
    /// projection's applied cursor; workflow-event cursors need only be
    /// monotonic because unrelated session facts may occupy the intervening
    /// coordinates and one fact may light more than one displayed node.
    /// `next_cursor` advances only after every change is admitted.
    pub fn apply_page(
        &mut self,
        page: WorkflowGraphWatchPage,
    ) -> Result<bool, WorkflowGraphProjectionError> {
        let Some(graph_id) = self.graph_id.as_deref() else {
            return Err(WorkflowGraphProjectionError::GraphChanged {
                expected: "<snapshot required>".to_owned(),
                observed: page.graph_id,
            });
        };
        if graph_id != page.graph_id {
            return Err(WorkflowGraphProjectionError::GraphChanged {
                expected: graph_id.to_owned(),
                observed: page.graph_id,
            });
        }
        let Some(workflow_id) = self.workflow_id.as_deref() else {
            return Err(WorkflowGraphProjectionError::WorkflowChanged {
                expected: "<snapshot required>".to_owned(),
                observed: page.workflow_id,
            });
        };
        if workflow_id != page.workflow_id {
            return Err(WorkflowGraphProjectionError::WorkflowChanged {
                expected: workflow_id.to_owned(),
                observed: page.workflow_id,
            });
        }
        let workflow_digest = self.workflow_digest.as_deref().unwrap_or_default();
        if workflow_digest != page.workflow_digest {
            return Err(WorkflowGraphProjectionError::DigestChanged {
                expected: workflow_digest.to_owned(),
                observed: page.workflow_digest,
            });
        }
        let current = self.cursor.unwrap_or(0);
        if page.next_cursor <= current && page.requested_after_cursor < current {
            return Ok(false);
        }
        if page.requested_after_cursor != current {
            return Err(WorkflowGraphProjectionError::CursorMismatch {
                applied: current,
                requested_after: page.requested_after_cursor,
            });
        }
        if page.replay_through_cursor < current
            || page.next_cursor < current
            || page.next_cursor > page.replay_through_cursor
        {
            return Err(WorkflowGraphProjectionError::InvalidWatchPage(
                "cursor bounds are inconsistent",
            ));
        }
        let mut nodes = self.nodes.clone();
        let mut event_cursor = current;
        for change in page.changes {
            if change.cursor <= current
                || change.cursor < event_cursor
                || change.cursor > page.replay_through_cursor
            {
                return Err(WorkflowGraphProjectionError::InvalidWatchPage(
                    "event cursors do not follow the request or exceed the replay bound",
                ));
            }
            if change.node.node_id.is_empty() {
                return Err(WorkflowGraphProjectionError::EmptyNodeId);
            }
            if !nodes.contains_key(&change.node.node_id) {
                return Err(WorkflowGraphProjectionError::UnknownNode {
                    node_id: change.node.node_id,
                });
            }
            event_cursor = change.cursor;
            nodes.insert(change.node.node_id.clone(), change.node);
        }
        if page.next_cursor < event_cursor {
            return Err(WorkflowGraphProjectionError::InvalidWatchPage(
                "next cursor precedes an applied event",
            ));
        }
        self.nodes = nodes;
        self.cursor = Some(page.next_cursor);
        Ok(page.next_cursor > current)
    }

    /// Evidence coordinates for a rejected node. Non-rejected nodes never
    /// masquerade as inspectable failures.
    #[must_use]
    pub fn rejection_evidence(&self, node_id: &str) -> Option<&[WorkflowEvidenceRef]> {
        self.node(node_id)
            .filter(|node| node.status == WorkflowNodeState::Rejected)
            .map(|node| node.evidence_refs.as_slice())
    }

    #[must_use]
    pub fn rejection(&self, node_id: &str) -> Option<&WorkflowNodeRejection> {
        self.node(node_id)
            .filter(|node| node.status == WorkflowNodeState::Rejected)
            .and_then(|node| node.rejection.as_ref())
    }
}

fn validate_edges(
    edges: &[WorkflowGraphEdge],
    node_ids: &BTreeSet<String>,
) -> Result<(), WorkflowGraphProjectionError> {
    for edge in edges {
        if !node_ids.contains(&edge.to) {
            return Err(WorkflowGraphProjectionError::InvalidTopology(
                "edge target is absent from the node set",
            ));
        }
        match edge.kind {
            WorkflowGraphEdgeKind::GraphInput if edge.from.is_some() => {
                return Err(WorkflowGraphProjectionError::InvalidTopology(
                    "graph input edge has a source node",
                ));
            }
            WorkflowGraphEdgeKind::Forward | WorkflowGraphEdgeKind::Back => {
                let Some(source) = edge.from.as_ref() else {
                    return Err(WorkflowGraphProjectionError::InvalidTopology(
                        "runtime edge has no source node",
                    ));
                };
                if !node_ids.contains(source) {
                    return Err(WorkflowGraphProjectionError::InvalidTopology(
                        "edge source is absent from the node set",
                    ));
                }
            }
            WorkflowGraphEdgeKind::GraphInput => {}
        }
    }
    Ok(())
}
