//! Convergence Graph M1 contracts and pure journal reduction.
//!
//! A graph is an immutable template instance. Executors never advance it:
//! they append [`EvidenceRecorded`] facts and the daemon alone appends gate,
//! advancement, retry, blocking, and completion facts. M2a adds declared
//! replacement slots and daemon-recorded process signals while retaining the
//! exact flat-counter branch for legacy pinned instances.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::ids::{
    AgentId, ArtifactRef, BranchId, EffectId, EventId, GraphId, GraphRunSetId, ItemId, MenuId,
    RunId, SessionId, WorkspaceRevision,
};
use crate::pipe::InstructEvidenceRef;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;

pub const SHIP_LOOP_TEMPLATE: &str = "ship-loop";
pub const SUPER_SHIP_LOOP_TEMPLATE: &str = "super-ship-loop";
pub const STAGGERED_TEMPLATE: &str = "staggered";
pub const SEC_AUDIT_TEMPLATE: &str = "sec-audit";
pub const DOCS_SWEEP_TEMPLATE: &str = "docs-sweep";
pub const IMPLEMENT_VERIFY_CHILD_TEMPLATE: &str = "child-implement-verify";
pub const DEEPER_CHILD_TEMPLATE: &str = "child-deeper";
pub const GRAPH_TEMPLATE_VERSION: u32 = 1;
pub const GRAPH_NODE_NAME_MAX_BYTES: usize = 64;
pub const GRAPH_MAX_NODES: usize = 512;
pub const GRAPH_MAX_EDGES: usize = 4_096;
pub const GRAPH_MAX_SLOTS: usize = 4_096;
pub const GRAPH_MAX_ATTEMPTS: u32 = 8;
/// Maximum conditional self/back hops in one immutable graph instance.
/// Forward DAG traversal does not consume this budget.
pub const GRAPH_MAX_CONDITIONAL_HOPS: u32 = 24;
pub const GRAPH_MAX_EVIDENCE_PER_ATTEMPT: u32 = 8;
pub const GRAPH_EVIDENCE_DETAIL_MAX_BYTES: usize = 1_024;
pub const GRAPH_BRIEF_MAX_BYTES: usize = 400;
pub const GRAPH_INSPECT_MAX_PAGE: u32 = 100;
pub const GRAPH_INSPECT_MAX_RUNS: usize = 32;
pub const GRAPH_INSPECT_MAX_TOOL_SELECTION_ROWS: usize = 32;
pub const GRAPH_MAX_TODO_CHILDREN: usize = 50;
pub const GRAPH_TELEMETRY_MAX_RUN_ROWS: usize = 1_024;
pub const GRAPH_TELEMETRY_MAX_ATTEMPT_ROWS: usize = 4_096;
pub const GRAPH_TELEMETRY_MAX_TEMPLATE_ROWS: usize = 256;
/// Maximum activation-graph events returned by one cursor watch page.
pub const WORKFLOW_GRAPH_WATCH_MAX_EVENTS: u32 = 128;
pub const WORKFLOW_NODE_REJECT_MESSAGE_MAX_BYTES: usize = 1_024;

/// Bounded durable node identity. Node names are data, never control-flow
/// discriminants. The accepted wire form is one ASCII upper-case identifier
/// containing letters, digits, `_`, or `-`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GraphNodeName(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNodeNameError;

impl fmt::Display for GraphNodeNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "graph node name must match [A-Z][A-Z0-9_-]{{0,{}}}",
            GRAPH_NODE_NAME_MAX_BYTES - 1
        )
    }
}

impl std::error::Error for GraphNodeNameError {}

impl GraphNodeName {
    pub fn new(value: impl Into<String>) -> Result<Self, GraphNodeNameError> {
        let value = value.into();
        let mut bytes = value.bytes();
        let valid = !value.is_empty()
            && value.len() <= GRAPH_NODE_NAME_MAX_BYTES
            && bytes.next().is_some_and(|byte| byte.is_ascii_uppercase())
            && bytes.all(|byte| {
                byte.is_ascii_uppercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            });
        valid.then_some(Self(value)).ok_or(GraphNodeNameError)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn label(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for GraphNodeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for GraphNodeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GraphNodeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[must_use]
pub fn build_node() -> GraphNodeName {
    GraphNodeName("BUILD".into())
}

#[must_use]
pub fn verify_node() -> GraphNodeName {
    GraphNodeName("VERIFY".into())
}

#[must_use]
pub fn ship_node() -> GraphNodeName {
    GraphNodeName("SHIP".into())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GraphGateKind {
    CommandGreen,
    AllOfN { n: u32 },
    HumanConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphExecutorShape {
    Inline,
    FanOut,
    Human,
}

/// Who is allowed to establish the truth represented by one evidence slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAuthority {
    DaemonVerified,
    ModelAttested,
}

/// The subject a slot's evidence must remain bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectSelector {
    WorkspaceRevision,
    Command,
    Freeform,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceSlotSpec {
    pub id: String,
    pub authority: EvidenceAuthority,
    pub subject_selector: SubjectSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeSpec {
    pub name: GraphNodeName,
    pub gate: GraphGateKind,
    pub executor: GraphExecutorShape,
    pub max_attempts: u32,
    /// Maximum evidence items accepted before an unsatisfied attempt settles
    /// red and follows its declared retry target (or legacy START fallback).
    /// Human gates have no such round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_evidence_per_attempt: Option<u32>,
    /// Immutable incoming dependencies. M1's built-in template is a simple
    /// linear DAG, but the dependency shape is stamped rather than inferred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<GraphNodeName>,
    /// Target reopened when this node remains red after its bounded evidence
    /// round. Self is a ↻ retry; a transitive dependency ancestor is a ↺
    /// back-edge. Absence preserves the legacy whole-graph START retry law.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub red_target: Option<GraphNodeName>,
    /// Declared evidence frontiers for slot-aware `AllOfN` gates. Empty is
    /// the durable M1 discriminator and retains the legacy flat-counter law.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify_slots: Vec<EvidenceSlotSpec>,
}

/// One immutable catalog entry. `version` and `start_node` participate in its
/// digest together with every executable node field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphTemplateSpec {
    pub name: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_node: Option<GraphNodeName>,
    pub nodes: Vec<GraphNodeSpec>,
}

/// A typed runtime edge. Runtime ASTs use explicit input, forward, and
/// back-edge sources; no execution decision is inferred from source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEdgeKind {
    GraphInput,
    Forward,
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowActivationEdge {
    pub id: u32,
    pub kind: WorkflowEdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<GraphNodeName>,
    pub to: GraphNodeName,
    pub evidence_type: String,
}

/// Exact edge sets that can activate a node. The first activation requires
/// every `initial_all` edge; a later iteration requires one explicit back
/// edge from `reactivate_any`. This keeps fork/join and retry semantics typed
/// and inspectable instead of smuggling control flow through node order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowJoinSemantics {
    pub initial_all: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactivate_any: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowActivationNode {
    pub node: GraphNodeName,
    pub input_type: String,
    pub output_type: String,
    pub join: WorkflowJoinSemantics,
    #[serde(default, skip_serializing_if = "is_false")]
    pub convergence_gate: bool,
}

/// Immutable executable AST frozen into the journal when a registered Loom
/// workflow is pinned. This is the runtime structure of record; the older
/// GraphTemplateSpec remains the compatibility gate vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowActivationAst {
    pub workflow_id: String,
    pub workflow_digest: String,
    pub input_type: String,
    pub output_type: String,
    pub nodes: Vec<WorkflowActivationNode>,
    pub edges: Vec<WorkflowActivationEdge>,
    pub max_back_edge_activations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraphStarted {
    pub graph_id: GraphId,
    pub ast: WorkflowActivationAst,
    pub ast_digest: String,
    /// The exact external input is absent at pin time. It is captured from
    /// the first graph-input activation so pinning can expose a waiting AST
    /// without fabricating evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<InstructEvidenceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeInput {
    pub edge_id: u32,
    pub evidence: InstructEvidenceRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowActivationCause {
    ForwardJoin,
    BackEdge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeActivated {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    /// Node-local activation generation, starting at one.
    pub iteration: u32,
    /// Session-journal-stable total order across this graph instance.
    pub activation_order: u64,
    pub cause: WorkflowActivationCause,
    pub inputs: Vec<WorkflowNodeInput>,
    pub input_ledger_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowConvergenceStamp {
    pub decision_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeCompleted {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub iteration: u32,
    pub outputs: Vec<InstructEvidenceRef>,
    pub output_ledger_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convergence: Option<WorkflowConvergenceStamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeRejectCode {
    EvidenceRejected,
    TypedInputMissing,
    IterationGuard,
    ConvergenceRejected,
    Abandoned,
    Superseded,
    InvariantViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeRejected {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub iteration: u32,
    pub code: WorkflowNodeRejectCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<InstructEvidenceRef>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub convergence_gate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodePhase {
    Waiting,
    Activated,
    Completed,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeState {
    pub node: GraphNodeName,
    pub phase: WorkflowNodePhase,
    pub iteration: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_order: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<WorkflowNodeInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<InstructEvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convergence: Option<WorkflowConvergenceStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection: Option<WorkflowNodeRejected>,
    pub updated_cursor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGraphPhase {
    Active,
    Completed,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowActivationCoordinate {
    pub activation_order: u64,
    pub node: GraphNodeName,
    pub iteration: u32,
    pub cursor: u64,
}

/// Indexed read model derived exclusively from workflow activation facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraphState {
    pub graph_id: GraphId,
    pub ast: WorkflowActivationAst,
    pub ast_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<InstructEvidenceRef>,
    pub phase: WorkflowGraphPhase,
    pub through_cursor: u64,
    pub next_activation_order: u64,
    pub back_edge_activations: u32,
    pub nodes: Vec<WorkflowNodeState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activation_order: Vec<WorkflowActivationCoordinate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Additive activation-event family kept beside the frozen
/// [`crate::EventPayload`] union. Older live-view clients therefore ignore
/// these journal facts while graph-aware clients consume the dedicated state
/// and watch RPCs.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowGraphJournalEvent {
    WorkflowGraphStarted(Box<WorkflowGraphStarted>),
    WorkflowNodeActivated(WorkflowNodeActivated),
    WorkflowNodeCompleted(WorkflowNodeCompleted),
    WorkflowNodeRejected(WorkflowNodeRejected),
}

impl WorkflowGraphJournalEvent {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    pub fn from_payload_value(
        value: &serde_json::Value,
    ) -> Result<Option<Self>, serde_json::Error> {
        let known = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "workflow_graph_started"
                        | "workflow_node_activated"
                        | "workflow_node_completed"
                        | "workflow_node_rejected"
                )
            });
        if !known {
            return Ok(None);
        }
        serde_json::from_value(value.clone()).map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraphWatchEvent {
    pub cursor: u64,
    pub event: WorkflowGraphJournalEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGraphWatchPage {
    pub requested_after_cursor: u64,
    pub replay_through_cursor: u64,
    pub next_cursor: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<WorkflowGraphWatchEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowGraphReductionError {
    InvalidAst(String),
    InvalidEvent(String),
}

impl fmt::Display for WorkflowGraphReductionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAst(message) | Self::InvalidEvent(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for WorkflowGraphReductionError {}

impl WorkflowGraphState {
    pub fn from_started(
        cursor: u64,
        started: WorkflowGraphStarted,
    ) -> Result<Self, WorkflowGraphReductionError> {
        if cursor == 0 || started.graph_id.as_str().is_empty() {
            return Err(WorkflowGraphReductionError::InvalidEvent(
                "workflow graph start has invalid journal coordinates".into(),
            ));
        }
        validate_workflow_activation_ast(&started.ast)?;
        if let Some(seed) = started.seed.as_ref() {
            seed.validate().map_err(|error| {
                WorkflowGraphReductionError::InvalidEvent(format!(
                    "workflow graph seed is invalid: {error}"
                ))
            })?;
            if seed.evidence_type != started.ast.input_type || !seed.parents.is_empty() {
                return Err(WorkflowGraphReductionError::InvalidEvent(
                    "workflow graph seed type or root lineage differs from the AST input".into(),
                ));
            }
        }
        if started.ast_digest != workflow_activation_ast_digest(&started.ast) {
            return Err(WorkflowGraphReductionError::InvalidEvent(
                "workflow activation AST digest does not match".into(),
            ));
        }
        let nodes = started
            .ast
            .nodes
            .iter()
            .map(|node| WorkflowNodeState {
                node: node.node.clone(),
                phase: WorkflowNodePhase::Waiting,
                iteration: 0,
                activation_order: None,
                inputs: Vec::new(),
                outputs: Vec::new(),
                convergence: None,
                rejection: None,
                updated_cursor: cursor,
            })
            .collect();
        Ok(Self {
            graph_id: started.graph_id,
            ast: started.ast,
            ast_digest: started.ast_digest,
            seed: started.seed,
            phase: WorkflowGraphPhase::Active,
            through_cursor: cursor,
            next_activation_order: 1,
            back_edge_activations: 0,
            nodes,
            activation_order: Vec::new(),
        })
    }

    pub fn apply(
        &mut self,
        cursor: u64,
        event: &WorkflowGraphJournalEvent,
    ) -> Result<(), WorkflowGraphReductionError> {
        if cursor <= self.through_cursor {
            return Err(invalid_workflow_event(
                "workflow activation cursor did not advance",
            ));
        }
        match event {
            WorkflowGraphJournalEvent::WorkflowGraphStarted(_) => {
                return Err(invalid_workflow_event(
                    "workflow graph was started more than once",
                ));
            }
            WorkflowGraphJournalEvent::WorkflowNodeActivated(activated) => {
                self.apply_activation(cursor, activated)?;
            }
            WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed) => {
                self.apply_completion(cursor, completed)?;
            }
            WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected) => {
                self.apply_rejection(cursor, rejected)?;
            }
        }
        self.through_cursor = cursor;
        Ok(())
    }

    #[must_use]
    pub fn node(&self, node: &GraphNodeName) -> Option<&WorkflowNodeState> {
        self.nodes.iter().find(|candidate| candidate.node.eq(node))
    }

    /// Checks the indexed snapshot without consulting the journal. Stores
    /// use this at the projection trust boundary; replay remains the repair
    /// oracle, not the ordinary read path.
    pub fn validate_projection(&self) -> Result<(), WorkflowGraphReductionError> {
        validate_workflow_activation_ast(&self.ast)?;
        if let Some(seed) = self.seed.as_ref() {
            seed.validate().map_err(|error| {
                invalid_workflow_event(format!("workflow graph seed is invalid: {error}"))
            })?;
        }
        let expected_next_order = u64::try_from(self.activation_order.len())
            .ok()
            .and_then(|count| count.checked_add(1));
        if self.graph_id.as_str().is_empty()
            || self.through_cursor == 0
            || self.ast_digest != workflow_activation_ast_digest(&self.ast)
            || self.seed.as_ref().is_some_and(|seed| {
                seed.evidence_type != self.ast.input_type || !seed.parents.is_empty()
            })
            || (self.seed.is_none()
                && (!self.activation_order.is_empty()
                    || self.nodes.iter().any(|node| {
                        matches!(
                            node.phase,
                            WorkflowNodePhase::Activated | WorkflowNodePhase::Completed
                        )
                    })))
            || expected_next_order != Some(self.next_activation_order)
            || self.back_edge_activations > self.ast.max_back_edge_activations
            || self.nodes.len() != self.ast.nodes.len()
            || self.nodes.iter().zip(&self.ast.nodes).any(|(state, spec)| {
                state.node != spec.node
                    || state.updated_cursor == 0
                    || state.updated_cursor > self.through_cursor
                    || (state.phase != WorkflowNodePhase::Waiting && state.iteration == 0)
            })
        {
            return Err(invalid_workflow_event(
                "workflow graph projection identity, counters, or node index is invalid",
            ));
        }
        let mut previous_cursor = 0_u64;
        for (index, coordinate) in self.activation_order.iter().enumerate() {
            let expected_order = u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1));
            if expected_order != Some(coordinate.activation_order)
                || coordinate.iteration == 0
                || coordinate.cursor <= previous_cursor
                || coordinate.cursor > self.through_cursor
                || self.node(&coordinate.node).is_none()
            {
                return Err(invalid_workflow_event(
                    "workflow graph projection activation order is invalid",
                ));
            }
            previous_cursor = coordinate.cursor;
        }
        match self.phase {
            WorkflowGraphPhase::Completed if !self.terminal_nodes_completed() => {
                return Err(invalid_workflow_event(
                    "completed workflow graph projection has unfinished terminals",
                ));
            }
            WorkflowGraphPhase::Rejected
                if !self.nodes.iter().any(|node| node.rejection.is_some()) =>
            {
                return Err(invalid_workflow_event(
                    "rejected workflow graph projection has no inspectable rejection",
                ));
            }
            WorkflowGraphPhase::Active if self.terminal_nodes_completed() => {
                return Err(invalid_workflow_event(
                    "active workflow graph projection already completed every terminal",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_activation(
        &mut self,
        cursor: u64,
        activated: &WorkflowNodeActivated,
    ) -> Result<(), WorkflowGraphReductionError> {
        if activated.graph_id != self.graph_id
            || activated.activation_order != self.next_activation_order
            || self.phase == WorkflowGraphPhase::Completed
            || self.terminal_rejection_code().is_some()
        {
            return Err(invalid_workflow_event(
                "workflow node activation has wrong graph, total order, or terminal phase",
            ));
        }
        let spec = self
            .ast
            .nodes
            .iter()
            .find(|candidate| candidate.node == activated.node)
            .cloned()
            .ok_or_else(|| invalid_workflow_event("activated node is absent from the AST"))?;
        let previous_iteration = self
            .node(&activated.node)
            .map(|node| node.iteration)
            .ok_or_else(|| invalid_workflow_event("activated node has no projected state"))?;
        let expected_iteration = previous_iteration
            .checked_add(1)
            .ok_or_else(|| invalid_workflow_event("node activation iteration overflowed"))?;
        if activated.iteration != expected_iteration
            || activated.input_ledger_digest != workflow_input_ledger_digest(&activated.inputs)
        {
            return Err(invalid_workflow_event(
                "workflow activation iteration or input ledger is invalid",
            ));
        }
        for input in &activated.inputs {
            input.evidence.validate().map_err(|error| {
                invalid_workflow_event(format!("workflow activation input is invalid: {error}"))
            })?;
            let edge = self
                .ast
                .edges
                .iter()
                .find(|edge| edge.id == input.edge_id && edge.to == activated.node)
                .ok_or_else(|| invalid_workflow_event("activation names an unknown input edge"))?;
            if edge.evidence_type != input.evidence.evidence_type {
                return Err(invalid_workflow_event(
                    "activation input evidence has the wrong edge type",
                ));
            }
        }
        match activated.cause {
            WorkflowActivationCause::ForwardJoin => {
                if self
                    .node(&activated.node)
                    .is_none_or(|node| node.phase != WorkflowNodePhase::Waiting)
                    || edge_ids(&activated.inputs) != spec.join.initial_all
                    || !self.initial_inputs_exist(activated)?
                {
                    return Err(invalid_workflow_event(
                        "node activated before every typed initial input existed",
                    ));
                }
            }
            WorkflowActivationCause::BackEdge => {
                if self.seed.is_none()
                    || activated.iteration == 1
                    || activated.inputs.len() != 1
                    || !spec
                        .join
                        .reactivate_any
                        .contains(&activated.inputs[0].edge_id)
                    || !self.back_input_exists(&activated.inputs[0])
                {
                    return Err(invalid_workflow_event(
                        "node reactivation lacks a typed rejected back-edge input",
                    ));
                }
                let next = self.back_edge_activations.checked_add(1).ok_or_else(|| {
                    invalid_workflow_event("back-edge activation count overflowed")
                })?;
                if next > self.ast.max_back_edge_activations {
                    return Err(invalid_workflow_event(
                        "back-edge activation exceeded the bounded iteration guard",
                    ));
                }
                self.back_edge_activations = next;
                self.reset_descendants(&activated.node, cursor);
            }
        }
        if self.seed.is_none() {
            self.seed = activated.inputs.iter().find_map(|input| {
                self.ast
                    .edges
                    .iter()
                    .find(|edge| {
                        edge.id == input.edge_id && edge.kind == WorkflowEdgeKind::GraphInput
                    })
                    .map(|_| input.evidence.clone())
            });
        }
        let node = self
            .nodes
            .iter_mut()
            .find(|candidate| candidate.node == activated.node)
            .ok_or_else(|| invalid_workflow_event("activated node has no projected state"))?;
        if node.phase != WorkflowNodePhase::Waiting {
            return Err(invalid_workflow_event(
                "node activation requires a waiting projection",
            ));
        }
        node.phase = WorkflowNodePhase::Activated;
        node.iteration = activated.iteration;
        node.activation_order = Some(activated.activation_order);
        node.inputs.clone_from(&activated.inputs);
        node.outputs.clear();
        node.convergence = None;
        node.rejection = None;
        node.updated_cursor = cursor;
        self.activation_order.push(WorkflowActivationCoordinate {
            activation_order: activated.activation_order,
            node: activated.node.clone(),
            iteration: activated.iteration,
            cursor,
        });
        self.next_activation_order = self
            .next_activation_order
            .checked_add(1)
            .ok_or_else(|| invalid_workflow_event("activation total order overflowed"))?;
        self.phase = WorkflowGraphPhase::Active;
        Ok(())
    }

    fn apply_completion(
        &mut self,
        cursor: u64,
        completed: &WorkflowNodeCompleted,
    ) -> Result<(), WorkflowGraphReductionError> {
        if completed.graph_id != self.graph_id
            || completed.output_ledger_digest != workflow_evidence_ledger_digest(&completed.outputs)
            || self.terminal_rejection_code().is_some()
        {
            return Err(invalid_workflow_event(
                "workflow node completion has wrong graph or output ledger",
            ));
        }
        let spec = self
            .ast
            .nodes
            .iter()
            .find(|candidate| candidate.node == completed.node)
            .ok_or_else(|| invalid_workflow_event("completed node is absent from the AST"))?;
        if completed.outputs.len() != 1 || completed.outputs[0].evidence_type != spec.output_type {
            return Err(invalid_workflow_event(
                "completed node did not produce its exact typed output",
            ));
        }
        for output in &completed.outputs {
            output.validate().map_err(|error| {
                invalid_workflow_event(format!("workflow output evidence is invalid: {error}"))
            })?;
        }
        let node = self
            .nodes
            .iter_mut()
            .find(|candidate| candidate.node == completed.node)
            .ok_or_else(|| invalid_workflow_event("completed node has no projected state"))?;
        if node.phase != WorkflowNodePhase::Activated || node.iteration != completed.iteration {
            return Err(invalid_workflow_event(
                "only the current activated iteration may complete",
            ));
        }
        let expected_parents = node
            .inputs
            .iter()
            .map(|input| input.evidence.artifact.clone())
            .collect::<Vec<_>>();
        if completed.outputs[0].parents != expected_parents {
            return Err(invalid_workflow_event(
                "workflow output does not bind its ordered activation inputs",
            ));
        }
        if spec.convergence_gate != completed.convergence.is_some()
            || completed
                .convergence
                .as_ref()
                .is_some_and(|stamp| stamp.decision_digest != completed.outputs[0].ledger_digest)
        {
            return Err(invalid_workflow_event(
                "convergence gate completion lacks its output-bound inspectable stamp",
            ));
        }
        node.phase = WorkflowNodePhase::Completed;
        node.outputs.clone_from(&completed.outputs);
        node.convergence.clone_from(&completed.convergence);
        node.rejection = None;
        node.updated_cursor = cursor;
        if self.terminal_nodes_completed() {
            self.phase = WorkflowGraphPhase::Completed;
        }
        Ok(())
    }

    fn apply_rejection(
        &mut self,
        cursor: u64,
        rejected: &WorkflowNodeRejected,
    ) -> Result<(), WorkflowGraphReductionError> {
        let terminal_rejection = self.terminal_rejection_code();
        if rejected.graph_id != self.graph_id
            || rejected.message.trim().is_empty()
            || rejected.message.len() > WORKFLOW_NODE_REJECT_MESSAGE_MAX_BYTES
            || self.phase == WorkflowGraphPhase::Completed
            || terminal_rejection.is_some_and(|code| code != rejected.code)
        {
            return Err(invalid_workflow_event(
                "workflow node rejection has wrong graph, no detail, or a terminal phase",
            ));
        }
        if let Some(evidence) = &rejected.evidence {
            evidence.validate().map_err(|error| {
                invalid_workflow_event(format!("workflow reject evidence is invalid: {error}"))
            })?;
        }
        let spec = self
            .ast
            .nodes
            .iter()
            .find(|candidate| candidate.node == rejected.node)
            .ok_or_else(|| invalid_workflow_event("rejected node is absent from the AST"))?;
        if spec.convergence_gate != rejected.convergence_gate {
            return Err(invalid_workflow_event(
                "workflow rejection mislabels convergence authority",
            ));
        }
        let expected_parents = self
            .node(&rejected.node)
            .ok_or_else(|| invalid_workflow_event("rejected node has no projected state"))?
            .inputs
            .iter()
            .map(|input| input.evidence.artifact.clone())
            .collect::<Vec<_>>();
        if rejected
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.parents != expected_parents)
        {
            return Err(invalid_workflow_event(
                "workflow rejection evidence does not bind its activation inputs",
            ));
        }
        let node = self
            .nodes
            .iter_mut()
            .find(|candidate| candidate.node == rejected.node)
            .ok_or_else(|| invalid_workflow_event("rejected node has no projected state"))?;
        let next_iteration = node.iteration.checked_add(1);
        let rejects_missing_activation = next_iteration == Some(rejected.iteration)
            && match rejected.code {
                WorkflowNodeRejectCode::TypedInputMissing
                | WorkflowNodeRejectCode::InvariantViolation
                | WorkflowNodeRejectCode::Abandoned
                | WorkflowNodeRejectCode::Superseded => node.phase == WorkflowNodePhase::Waiting,
                // A guarded back hop can target the rejected source itself
                // or an already-completed ancestor. It is still a rejected
                // activation attempt and must be replayable journal truth.
                WorkflowNodeRejectCode::IterationGuard => {
                    node.phase != WorkflowNodePhase::Activated
                }
                _ => false,
            };
        if !rejects_missing_activation
            && (node.phase != WorkflowNodePhase::Activated || node.iteration != rejected.iteration)
        {
            return Err(invalid_workflow_event(
                "rejection does not name the current activation or a missing-input attempt",
            ));
        }
        node.phase = WorkflowNodePhase::Rejected;
        node.iteration = rejected.iteration;
        node.outputs.clear();
        node.convergence = None;
        node.rejection = Some(rejected.clone());
        node.updated_cursor = cursor;
        self.phase = WorkflowGraphPhase::Rejected;
        Ok(())
    }

    fn initial_inputs_exist(
        &self,
        activated: &WorkflowNodeActivated,
    ) -> Result<bool, WorkflowGraphReductionError> {
        for input in &activated.inputs {
            let edge = self
                .ast
                .edges
                .iter()
                .find(|edge| edge.id == input.edge_id)
                .ok_or_else(|| invalid_workflow_event("activation input edge disappeared"))?;
            let exists = match edge.kind {
                WorkflowEdgeKind::GraphInput => {
                    edge.from.is_none()
                        && self.seed.as_ref().map_or_else(
                            || {
                                input.evidence.evidence_type == self.ast.input_type
                                    && input.evidence.parents.is_empty()
                            },
                            |seed| input.evidence.eq(seed),
                        )
                }
                WorkflowEdgeKind::Forward => edge.from.as_ref().is_some_and(|source| {
                    self.node(source).is_some_and(|node| {
                        node.phase == WorkflowNodePhase::Completed
                            && node.outputs.contains(&input.evidence)
                    })
                }),
                WorkflowEdgeKind::Back => false,
            };
            if !exists {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn back_input_exists(&self, input: &WorkflowNodeInput) -> bool {
        let Some(edge) = self
            .ast
            .edges
            .iter()
            .find(|edge| edge.id == input.edge_id && edge.kind == WorkflowEdgeKind::Back)
        else {
            return false;
        };
        edge.from.as_ref().is_some_and(|source| {
            self.node(source)
                .and_then(|node| node.rejection.as_ref())
                .filter(|rejection| rejection.code == WorkflowNodeRejectCode::EvidenceRejected)
                .and_then(|reject| reject.evidence.as_ref())
                .is_some_and(|evidence| evidence == &input.evidence)
        })
    }

    fn reset_descendants(&mut self, target: &GraphNodeName, cursor: u64) {
        let mut reset = HashSet::from([target.clone()]);
        loop {
            let before = reset.len();
            for edge in self
                .ast
                .edges
                .iter()
                .filter(|edge| edge.kind == WorkflowEdgeKind::Forward)
            {
                if edge.from.as_ref().is_some_and(|from| reset.contains(from)) {
                    reset.insert(edge.to.clone());
                }
            }
            if before == reset.len() {
                break;
            }
        }
        for node in &mut self.nodes {
            if reset.contains(&node.node) {
                node.phase = WorkflowNodePhase::Waiting;
                node.activation_order = None;
                node.inputs.clear();
                node.outputs.clear();
                node.convergence = None;
                node.rejection = None;
                node.updated_cursor = cursor;
            }
        }
    }

    fn terminal_nodes_completed(&self) -> bool {
        self.ast
            .nodes
            .iter()
            .filter(|node| {
                !self.ast.edges.iter().any(|edge| {
                    edge.kind == WorkflowEdgeKind::Forward && edge.from.as_ref() == Some(&node.node)
                })
            })
            .all(|terminal| {
                self.node(&terminal.node)
                    .is_some_and(|state| state.phase == WorkflowNodePhase::Completed)
            })
    }

    fn terminal_rejection_code(&self) -> Option<WorkflowNodeRejectCode> {
        self.nodes.iter().find_map(|node| {
            node.rejection.as_ref().and_then(|rejection| {
                matches!(
                    rejection.code,
                    WorkflowNodeRejectCode::Abandoned | WorkflowNodeRejectCode::Superseded
                )
                .then_some(rejection.code)
            })
        })
    }
}

#[must_use]
pub fn workflow_activation_ast_digest(ast: &WorkflowActivationAst) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider/workflow-activation-ast/v1\0");
    hash_graph_part(&mut hasher, ast.workflow_id.as_bytes());
    hash_graph_part(&mut hasher, ast.workflow_digest.as_bytes());
    hash_graph_part(&mut hasher, ast.input_type.as_bytes());
    hash_graph_part(&mut hasher, ast.output_type.as_bytes());
    hash_graph_part(
        &mut hasher,
        &u64::try_from(ast.nodes.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for node in &ast.nodes {
        hash_graph_part(&mut hasher, node.node.as_str().as_bytes());
        hash_graph_part(&mut hasher, node.input_type.as_bytes());
        hash_graph_part(&mut hasher, node.output_type.as_bytes());
        hash_graph_part(&mut hasher, &[u8::from(node.convergence_gate)]);
        hash_graph_u32s(&mut hasher, &node.join.initial_all);
        hash_graph_u32s(&mut hasher, &node.join.reactivate_any);
    }
    hash_graph_part(
        &mut hasher,
        &u64::try_from(ast.edges.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for edge in &ast.edges {
        hash_graph_part(&mut hasher, &edge.id.to_be_bytes());
        hash_graph_part(
            &mut hasher,
            &[match edge.kind {
                WorkflowEdgeKind::GraphInput => 0,
                WorkflowEdgeKind::Forward => 1,
                WorkflowEdgeKind::Back => 2,
            }],
        );
        hash_graph_part(
            &mut hasher,
            edge.from
                .as_ref()
                .map_or(&[], |from| from.as_str().as_bytes()),
        );
        hash_graph_part(&mut hasher, edge.to.as_str().as_bytes());
        hash_graph_part(&mut hasher, edge.evidence_type.as_bytes());
    }
    hash_graph_part(&mut hasher, &ast.max_back_edge_activations.to_be_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[must_use]
pub fn workflow_input_ledger_digest(inputs: &[WorkflowNodeInput]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider/workflow-node-input-ledger/v1\0");
    for input in inputs {
        hash_graph_part(&mut hasher, &input.edge_id.to_be_bytes());
        hash_graph_part(&mut hasher, input.evidence.ledger_digest.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[must_use]
pub fn workflow_evidence_ledger_digest(outputs: &[InstructEvidenceRef]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider/workflow-node-output-ledger/v1\0");
    for output in outputs {
        hash_graph_part(&mut hasher, output.ledger_digest.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

pub fn validate_workflow_activation_ast(
    ast: &WorkflowActivationAst,
) -> Result<(), WorkflowGraphReductionError> {
    if ast.workflow_id.is_empty()
        || ast.workflow_digest.is_empty()
        || !valid_workflow_evidence_type(&ast.input_type)
        || !valid_workflow_evidence_type(&ast.output_type)
        || ast.nodes.is_empty()
        || ast.nodes.len() > GRAPH_MAX_NODES
        || ast.edges.is_empty()
        || ast.edges.len() > GRAPH_MAX_EDGES
        || ast.max_back_edge_activations == 0
        || ast.max_back_edge_activations > GRAPH_MAX_CONDITIONAL_HOPS
    {
        return Err(invalid_workflow_ast(
            "workflow activation AST identity, bounds, or types are invalid",
        ));
    }
    let nodes = ast
        .nodes
        .iter()
        .map(|node| node.node.clone())
        .collect::<HashSet<_>>();
    if nodes.len() != ast.nodes.len() {
        return Err(invalid_workflow_ast(
            "workflow activation AST contains duplicate nodes",
        ));
    }
    let edge_ids = ast.edges.iter().map(|edge| edge.id).collect::<HashSet<_>>();
    let graph_input_count = ast
        .edges
        .iter()
        .filter(|edge| edge.kind == WorkflowEdgeKind::GraphInput)
        .count();
    if edge_ids.len() != ast.edges.len() || edge_ids.contains(&0) || graph_input_count != 1 {
        return Err(invalid_workflow_ast(
            "workflow activation AST needs unique nonzero edges and exactly one graph input",
        ));
    }
    for edge in &ast.edges {
        if !nodes.contains(&edge.to) || !valid_workflow_evidence_type(&edge.evidence_type) {
            return Err(invalid_workflow_ast(
                "workflow activation edge target or type is invalid",
            ));
        }
        let target = ast
            .nodes
            .iter()
            .find(|node| node.node == edge.to)
            .ok_or_else(|| invalid_workflow_ast("workflow activation edge lost its target"))?;
        match edge.kind {
            WorkflowEdgeKind::GraphInput
                if edge.from.is_none() && edge.evidence_type == ast.input_type => {}
            WorkflowEdgeKind::Forward
                if edge.from.as_ref().is_some_and(|from| {
                    let source = ast.nodes.iter().position(|node| node.node.eq(from));
                    let target = ast.nodes.iter().position(|node| node.node == edge.to);
                    source
                        .zip(target)
                        .is_some_and(|(source_index, target_index)| {
                            source_index < target_index
                                && ast.nodes[source_index].output_type == edge.evidence_type
                        })
                }) => {}
            WorkflowEdgeKind::Back
                if edge
                    .from
                    .as_ref()
                    .is_some_and(|from| workflow_forward_reaches(ast, &edge.to, from))
                    && edge.evidence_type == target.input_type => {}
            _ => {
                return Err(invalid_workflow_ast(
                    "workflow activation edge has an invalid source",
                ));
            }
        }
    }
    for node in &ast.nodes {
        let incoming_types = ast
            .edges
            .iter()
            .filter(|edge| edge.to == node.node && edge.kind != WorkflowEdgeKind::Back)
            .map(|edge| edge.evidence_type.clone())
            .collect::<Vec<_>>();
        let expected_initial = ast
            .edges
            .iter()
            .filter(|edge| edge.to == node.node && edge.kind != WorkflowEdgeKind::Back)
            .map(|edge| edge.id)
            .collect::<Vec<_>>();
        let expected_back = ast
            .edges
            .iter()
            .filter(|edge| edge.to == node.node && edge.kind == WorkflowEdgeKind::Back)
            .map(|edge| edge.id)
            .collect::<Vec<_>>();
        let unique_initial = node
            .join
            .initial_all
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let unique_back = node
            .join
            .reactivate_any
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if !valid_workflow_evidence_type(&node.input_type)
            || !valid_workflow_evidence_type(&node.output_type)
            || node.join.initial_all.is_empty()
            || unique_initial.len() != node.join.initial_all.len()
            || unique_back.len() != node.join.reactivate_any.len()
            || node.join.initial_all != expected_initial
            || node.join.reactivate_any != expected_back
            || !workflow_node_accepts_inputs(&node.input_type, &incoming_types)
        {
            return Err(invalid_workflow_ast(
                "workflow node join or typed signature is invalid",
            ));
        }
    }
    let terminals = ast
        .nodes
        .iter()
        .filter(|node| {
            !ast.edges.iter().any(|edge| {
                edge.kind == WorkflowEdgeKind::Forward && edge.from.as_ref() == Some(&node.node)
            })
        })
        .collect::<Vec<_>>();
    let terminal_types = terminals
        .iter()
        .map(|node| node.output_type.clone())
        .collect::<Vec<_>>();
    if terminals.is_empty() || !workflow_output_accepts(&ast.output_type, &terminal_types) {
        return Err(invalid_workflow_ast(
            "workflow activation terminals do not produce the workflow output type",
        ));
    }
    Ok(())
}

fn workflow_forward_reaches(
    ast: &WorkflowActivationAst,
    ancestor: &GraphNodeName,
    descendant: &GraphNodeName,
) -> bool {
    if ancestor == descendant {
        return true;
    }
    let mut pending = VecDeque::from([ancestor.clone()]);
    let mut visited = HashSet::from([ancestor.clone()]);
    while let Some(source) = pending.pop_front() {
        for target in ast.edges.iter().filter_map(|edge| {
            (edge.kind == WorkflowEdgeKind::Forward && edge.from.as_ref() == Some(&source))
                .then_some(&edge.to)
        }) {
            if target == descendant {
                return true;
            }
            if visited.insert(target.clone()) {
                pending.push_back(target.clone());
            }
        }
    }
    false
}

/// Materializes the immutable Loom record into the executable runtime AST.
/// This consumes compiled metadata only; it never reparses or reinterprets
/// authoring source.
pub fn workflow_activation_ast_from_loom(
    workflow: &crate::loom::LoomWorkflow,
) -> Result<WorkflowActivationAst, WorkflowGraphReductionError> {
    if workflow.template.nodes.len() != workflow.meta.len() {
        return Err(invalid_workflow_ast(
            "compiled workflow metadata does not cover every template node",
        ));
    }
    let start =
        workflow.template.start_node.as_ref().ok_or_else(|| {
            invalid_workflow_ast("compiled workflow has no activation start node")
        })?;
    let mut next_edge_id = 1_u32;
    let mut edges = Vec::new();
    let mut nodes = Vec::new();
    let mut output_types = BTreeMap::<GraphNodeName, String>::new();
    for spec in &workflow.template.nodes {
        let meta = workflow
            .meta
            .iter()
            .find(|meta| meta.node == spec.name)
            .ok_or_else(|| invalid_workflow_ast("compiled workflow node metadata is missing"))?;
        let incoming = if spec.depends_on.is_empty() {
            vec![(None, workflow.in_type.clone())]
        } else {
            let mut incoming = Vec::with_capacity(spec.depends_on.len());
            for dependency in &spec.depends_on {
                let evidence_type = output_types.get(dependency).cloned().ok_or_else(|| {
                    invalid_workflow_ast("compiled workflow dependency is not in topological order")
                })?;
                incoming.push((Some(dependency.clone()), evidence_type));
            }
            incoming
        };
        let carried = merge_workflow_types(
            &incoming
                .iter()
                .map(|(_, evidence_type)| evidence_type.clone())
                .collect::<Vec<_>>(),
        );
        let input_type = meta.in_type.clone().unwrap_or_else(|| carried.clone());
        let output_type = meta.out_type.clone().unwrap_or(carried);
        let mut initial_all = Vec::with_capacity(incoming.len());
        for (from, evidence_type) in incoming {
            let id = next_edge_id;
            next_edge_id = next_edge_id
                .checked_add(1)
                .ok_or_else(|| invalid_workflow_ast("workflow edge id space is exhausted"))?;
            initial_all.push(id);
            edges.push(WorkflowActivationEdge {
                id,
                kind: if from.is_some() {
                    WorkflowEdgeKind::Forward
                } else {
                    WorkflowEdgeKind::GraphInput
                },
                from,
                to: spec.name.clone(),
                evidence_type,
            });
        }
        nodes.push(WorkflowActivationNode {
            node: spec.name.clone(),
            input_type,
            output_type: output_type.clone(),
            join: WorkflowJoinSemantics {
                initial_all,
                reactivate_any: Vec::new(),
            },
            // Human decisions are explicit convergence points. Successful
            // terminal gates also decide the workflow result and therefore
            // carry an output-bound convergence stamp.
            convergence_gate: matches!(spec.gate, GraphGateKind::HumanConfirm)
                || !workflow
                    .template
                    .nodes
                    .iter()
                    .any(|candidate| candidate.depends_on.contains(&spec.name)),
        });
        output_types.insert(spec.name.clone(), output_type);
    }
    for spec in workflow
        .template
        .nodes
        .iter()
        .filter(|spec| !matches!(spec.gate, GraphGateKind::HumanConfirm))
    {
        let target = spec.red_target.as_ref().unwrap_or(start);
        let target_index = nodes
            .iter()
            .position(|node| node.node.eq(target))
            .ok_or_else(|| invalid_workflow_ast("workflow back edge has no target node"))?;
        let evidence_type = nodes[target_index].input_type.clone();
        let id = next_edge_id;
        next_edge_id = next_edge_id
            .checked_add(1)
            .ok_or_else(|| invalid_workflow_ast("workflow edge id space is exhausted"))?;
        edges.push(WorkflowActivationEdge {
            id,
            kind: WorkflowEdgeKind::Back,
            from: Some(spec.name.clone()),
            to: target.clone(),
            evidence_type,
        });
        nodes[target_index].join.reactivate_any.push(id);
    }
    let ast = WorkflowActivationAst {
        workflow_id: workflow.id.clone(),
        workflow_digest: workflow.digest.clone(),
        input_type: workflow.in_type.clone(),
        output_type: workflow.out_type.clone(),
        nodes,
        edges,
        max_back_edge_activations: GRAPH_MAX_CONDITIONAL_HOPS,
    };
    validate_workflow_activation_ast(&ast)?;
    Ok(ast)
}

fn merge_workflow_types(inputs: &[String]) -> String {
    if let [only] = inputs {
        return only.clone();
    }
    let mut merged = Vec::<&str>::new();
    for input in inputs {
        for operand in input
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            if !merged.contains(&operand) {
                merged.push(operand);
            }
        }
    }
    merged.join(" + ")
}

fn workflow_node_accepts_inputs(expected: &str, inputs: &[String]) -> bool {
    let carried = merge_workflow_types(inputs);
    if inputs.len() > 1 {
        workflow_type_operands(expected) == workflow_type_operands(&carried)
    } else {
        expected == carried
            || expected
                .split('+')
                .map(str::trim)
                .any(|operand| operand == carried)
    }
}

fn workflow_output_accepts(expected: &str, terminal_types: &[String]) -> bool {
    let produced = merge_workflow_types(terminal_types);
    if terminal_types.len() > 1 {
        workflow_type_operands(expected) == workflow_type_operands(&produced)
    } else {
        workflow_node_accepts_inputs(expected, terminal_types)
    }
}

fn workflow_type_operands(value: &str) -> Vec<&str> {
    let mut operands = value
        .split('+')
        .map(str::trim)
        .filter(|operand| !operand.is_empty())
        .collect::<Vec<_>>();
    operands.sort_unstable();
    operands.dedup();
    operands
}

fn valid_workflow_evidence_type(value: &str) -> bool {
    crate::loom::valid_type_expr(value)
}

/// Strict from-scratch reducer used for replay verification and projection
/// repair proofs. Unknown/non-activation facts are ignored; malformed known
/// activation facts fail closed with their journal coordinates.
pub fn reduce_workflow_graphs(
    envelopes: &[RawEnvelope],
) -> Result<HashMap<GraphId, WorkflowGraphState>, WorkflowGraphReductionError> {
    let mut states = HashMap::new();
    for envelope in envelopes {
        let event = match WorkflowGraphJournalEvent::from_payload_value(&envelope.payload) {
            Ok(Some(event)) => event,
            Ok(None) => continue,
            Err(error) => {
                return Err(invalid_workflow_event(format!(
                    "malformed workflow activation fact at cursor {}: {error}",
                    envelope.seq
                )));
            }
        };
        let event = match event {
            WorkflowGraphJournalEvent::WorkflowGraphStarted(started) => {
                let graph_id = started.graph_id.clone();
                if states.contains_key(&graph_id) {
                    return Err(invalid_workflow_event(format!(
                        "workflow graph {graph_id} started twice at cursor {}",
                        envelope.seq
                    )));
                }
                states.insert(
                    graph_id,
                    WorkflowGraphState::from_started(envelope.seq, *started)?,
                );
                continue;
            }
            other => other,
        };
        let graph_id = match &event {
            WorkflowGraphJournalEvent::WorkflowGraphStarted(started) => &started.graph_id,
            WorkflowGraphJournalEvent::WorkflowNodeActivated(activated) => &activated.graph_id,
            WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed) => &completed.graph_id,
            WorkflowGraphJournalEvent::WorkflowNodeRejected(rejected) => &rejected.graph_id,
        };
        let state = states.get_mut(graph_id).ok_or_else(|| {
            invalid_workflow_event(format!(
                "workflow activation fact for {graph_id} precedes its start at cursor {}",
                envelope.seq
            ))
        })?;
        state.apply(envelope.seq, &event)?;
    }
    Ok(states)
}

fn edge_ids(inputs: &[WorkflowNodeInput]) -> Vec<u32> {
    inputs.iter().map(|input| input.edge_id).collect()
}

fn hash_graph_u32s(hasher: &mut blake3::Hasher, values: &[u32]) {
    hash_graph_part(
        hasher,
        &u64::try_from(values.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for value in values {
        hash_graph_part(hasher, &value.to_be_bytes());
    }
}

fn hash_graph_part(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(bytes);
}

fn invalid_workflow_ast(message: impl Into<String>) -> WorkflowGraphReductionError {
    WorkflowGraphReductionError::InvalidAst(message.into())
}

fn invalid_workflow_event(message: impl Into<String>) -> WorkflowGraphReductionError {
    WorkflowGraphReductionError::InvalidEvent(message.into())
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// One release-owned workflow and its selection class. Eligibility describes
/// where the workflow may be offered; it is not graph execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltInWorkflowCatalogEntry {
    pub template: GraphTemplateSpec,
    pub main_session_eligible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphTemplateRejection {
    DuplicateNode,
    NoStart,
    MultipleStarts,
    Cycle,
    UnreachableNode,
    UnknownDependency,
    OverCeiling,
    InvalidGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphTemplateError {
    pub kind: GraphTemplateRejection,
    pub message: String,
}

impl fmt::Display for GraphTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GraphTemplateError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphPinned {
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
    /// Zero is the durable pre-M2b discriminator.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub template_version: u32,
    /// Missing only on legacy pins where BUILD was implicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_node: Option<GraphNodeName>,
    pub nodes: Vec<GraphNodeSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphAttemptOpened {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    /// Monotonic traversal epoch. A conditional hop increments it; nodes
    /// reached afterward carry the new epoch while unaffected fork siblings
    /// may retain an older node-local attempt.
    pub attempt: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceVerdict {
    Green,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphEvidenceSource {
    Model {
        run_id: RunId,
        call_id: String,
    },
    ProcessSignal {
        run_id: RunId,
        call_id: String,
        effect_id: EffectId,
    },
    WorkspaceMutation {
        run_id: RunId,
        effect_id: EffectId,
    },
    /// A screen observation produced and admitted by the daemon-owned
    /// computer backend. This source is never accepted from the
    /// model-callable `graph_evidence` tool: the daemon stamps the current
    /// workspace revision only after the redacted image has entered the CAS.
    ComputerObservation {
        run_id: RunId,
        call_id: String,
        effect_id: EffectId,
        effect_args_digest: String,
        observation: ComputerObservationKind,
        image: crate::tool::ImageBlockRef,
        workspace_revision: WorkspaceRevision,
    },
    /// One terminal delegated workflow collapsed at the parent graph
    /// boundary. The daemon validates these coordinates against both the
    /// delegation record and [`ChildGraphAttached`] before accepting it.
    ChildContract {
        child_session_id: SessionId,
        child_run_id: RunId,
        child_graph_id: GraphId,
        report_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_revision: Option<WorkspaceRevision>,
    },
}

/// The two computer observations that can carry daemon-authenticated screen
/// evidence. This is graph provenance, not an extension of `ComputerAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerObservationKind {
    Screenshot,
    Inspect,
}

/// Binds the exact redacted CAS image and observation coordinates to the
/// daemon-observed workspace revision. Length-prefixing keeps this digest
/// domain independent from process and workspace-mutation subjects.
#[must_use]
pub fn computer_observation_subject_digest(
    run_id: &RunId,
    call_id: &str,
    effect_id: &EffectId,
    effect_args_digest: &str,
    observation: ComputerObservationKind,
    image: &crate::tool::ImageBlockRef,
    workspace_revision: &WorkspaceRevision,
) -> String {
    let observation = match observation {
        ComputerObservationKind::Screenshot => "screenshot",
        ComputerObservationKind::Inspect => "inspect",
    };
    let dimensions = format!("{}x{}", image.width, image.height);
    let byte_len = image.byte_len.to_string();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.computer-observation.subject.v1");
    for value in [
        run_id.as_str(),
        call_id,
        effect_id.as_str(),
        effect_args_digest,
        observation,
        image.artifact.as_str(),
        image.media_type.as_str(),
        dimensions.as_str(),
        byte_len.as_str(),
        workspace_revision.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Stable coordinates submitted by `graph_evidence` for daemon-verified
/// process truth. The daemon resolves all three fields against the journal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessSignalRef {
    pub run_id: RunId,
    pub call_id: String,
    pub effect_id: EffectId,
}

/// Stable coordinates submitted by `graph_evidence` for a daemon-stamped
/// workspace mutation (for example `fs_write`, `fs_edit`, or `fs_path`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceMutationRef {
    pub run_id: RunId,
    pub effect_id: EffectId,
}

/// Daemon-internal reference used to collapse one terminal child workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildContractRef {
    pub child_session_id: SessionId,
    pub child_run_id: RunId,
    pub child_graph_id: GraphId,
    pub report_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
}

#[must_use]
pub fn child_contract_subject_digest(contract: &ChildContractRef) -> String {
    let bytes = serde_json::to_vec(contract).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

/// Binds one mutation effect and its post-state digest to the exact monotonic
/// revision assigned by the store. Length-prefixing prevents ambiguous
/// concatenations and keeps the digest domain independent from process
/// subjects.
#[must_use]
pub fn workspace_mutation_subject_digest(
    effect_id: &EffectId,
    mutation_digest: &str,
    workspace_revision: &WorkspaceRevision,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.workspace-mutation.subject.v1");
    for value in [
        effect_id.as_str(),
        mutation_digest,
        workspace_revision.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Daemon-observed terminal truth for one process execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSignalRecorded {
    pub run_id: RunId,
    pub call_id: String,
    pub effect_id: EffectId,
    pub command_arg_digest: String,
    pub exit_code: Option<i32>,
    pub transcript_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
    pub subject_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecorded {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    /// The target node's open traversal epoch, implied by its obligation at
    /// tool-call time and stamped by the daemon.
    pub attempt: u32,
    pub verdict: EvidenceVerdict,
    pub detail: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    pub source: GraphEvidenceSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphGateSatisfied {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphAdvanced {
    pub graph_id: GraphId,
    pub from_node: GraphNodeName,
    pub to_node: GraphNodeName,
}

/// Dependency-engine readiness fact. Linear legacy-compatible templates keep
/// emitting `GraphAdvanced`; every non-linear opening emits this variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeReadied {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSuperseded {
    pub old: GraphId,
    pub new: GraphId,
}

/// Opens one immutable aggregate over the todo list carried by a particular
/// Plan fact. `plan_event_id` disambiguates whole-list replacements that reuse
/// the same G1 Plan `ItemId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRunSetOpened {
    pub run_set_id: GraphRunSetId,
    pub root_graph_id: GraphId,
    pub plan_item_id: ItemId,
    pub plan_event_id: EventId,
    pub required_children: u32,
}

/// Immutable binding between one todo identity and one child graph. Dependency
/// coordinates are frozen here, so a later reordered replacement list cannot
/// retarget an existing child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoGraphAttached {
    pub run_set_id: GraphRunSetId,
    pub plan_item_id: ItemId,
    pub todo_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_todo_id: Option<u32>,
    pub child_graph_id: GraphId,
    pub ordinal: u32,
}

/// Optional workflow selector accepted by `spawn_subagent`. Its wire shape is
/// deliberately one bounded string so adding it does not perturb legacy tool
/// arguments or receipts when omitted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChildWorkflowSelector {
    Plain,
    ImplementVerify,
    Deeper,
    WorkflowRef(String),
}

impl ChildWorkflowSelector {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "plain" => Ok(Self::Plain),
            "implement_verify" => Ok(Self::ImplementVerify),
            "deeper" => Ok(Self::Deeper),
            _ => value
                .strip_prefix("workflow_ref(")
                .and_then(|rest| rest.strip_suffix(')'))
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= GRAPH_NODE_NAME_MAX_BYTES
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
                .map(|name| Self::WorkflowRef(name.to_owned()))
                .ok_or_else(|| {
                    "workflow must be plain, implement_verify, deeper, or workflow_ref(<name>)"
                        .to_owned()
                }),
        }
    }

    #[must_use]
    pub fn template_name(&self) -> Option<&str> {
        match self {
            Self::Plain => None,
            Self::ImplementVerify => Some(IMPLEMENT_VERIFY_CHILD_TEMPLATE),
            Self::Deeper => Some(DEEPER_CHILD_TEMPLATE),
            Self::WorkflowRef(name) => Some(name),
        }
    }
}

impl fmt::Display for ChildWorkflowSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => formatter.write_str("plain"),
            Self::ImplementVerify => formatter.write_str("implement_verify"),
            Self::Deeper => formatter.write_str("deeper"),
            Self::WorkflowRef(name) => write!(formatter, "workflow_ref({name})"),
        }
    }
}

impl Serialize for ChildWorkflowSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ChildWorkflowSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// The bounded facts which can justify graph ceremony for one child. The
/// model declares one; the daemon applies the deterministic gate below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildWorkflowTrigger {
    MutationWithIndependentVerification,
    DependentPhases,
    FanOut,
    DistinctReview,
    CrashRecovery,
}

impl ChildWorkflowTrigger {
    #[must_use]
    pub fn is_deeper(self) -> bool {
        matches!(
            self,
            Self::DependentPhases | Self::FanOut | Self::DistinctReview | Self::CrashRecovery
        )
    }
}

/// Auditable result of the sparse daemon decision gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildWorkflowDecision {
    pub requested: ChildWorkflowSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<ChildWorkflowTrigger>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub workflow_author: bool,
}

impl ChildWorkflowDecision {
    #[must_use]
    pub fn is_bare(&self) -> bool {
        self.template.is_none()
    }
}

/// The gate is intentionally biased toward no graph. An explicit selector is
/// only a proposal: without the matching bounded trigger it collapses to one
/// ordinary attempt.
#[must_use]
pub fn decide_child_workflow(
    requested: Option<&ChildWorkflowSelector>,
    trigger: Option<ChildWorkflowTrigger>,
    workflow_author_requested: bool,
) -> ChildWorkflowDecision {
    decide_child_workflow_with_registry(requested, trigger, workflow_author_requested, false)
}

/// Applies the child-workflow gate with an optional positive registry
/// resolution for a [`ChildWorkflowSelector::WorkflowRef`]. The registry is
/// deliberately consulted by the daemon before this pure decision: unknown
/// names retain the historical bare-attempt fallback, while a registered Loom
/// workflow follows the same explicit-trigger and workflow-author laws as the
/// built-in child templates.
#[must_use]
pub fn decide_child_workflow_with_registry(
    requested: Option<&ChildWorkflowSelector>,
    trigger: Option<ChildWorkflowTrigger>,
    workflow_author_requested: bool,
    registered_workflow_ref: bool,
) -> ChildWorkflowDecision {
    let requested = requested.cloned().unwrap_or(ChildWorkflowSelector::Plain);
    let (template, reason) = match (&requested, trigger) {
        (ChildWorkflowSelector::Plain, _) => (None, "default_bare_attempt"),
        (
            ChildWorkflowSelector::ImplementVerify,
            Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
        ) => (
            requested.template_name().map(str::to_owned),
            "mutation_with_independent_verification",
        ),
        (ChildWorkflowSelector::ImplementVerify, _) => {
            (None, "missing_mutation_independent_verification")
        }
        (ChildWorkflowSelector::Deeper, Some(found)) if found.is_deeper() => (
            requested.template_name().map(str::to_owned),
            match found {
                ChildWorkflowTrigger::DependentPhases => "dependent_phases",
                ChildWorkflowTrigger::FanOut => "fan_out",
                ChildWorkflowTrigger::DistinctReview => "distinct_review",
                ChildWorkflowTrigger::CrashRecovery => "crash_recovery",
                ChildWorkflowTrigger::MutationWithIndependentVerification => unreachable!(),
            },
        ),
        (ChildWorkflowSelector::Deeper, _) => (None, "missing_deeper_workflow_trigger"),
        (
            ChildWorkflowSelector::WorkflowRef(name),
            Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
        ) if name == IMPLEMENT_VERIFY_CHILD_TEMPLATE => (
            Some(name.clone()),
            "workflow_ref_mutation_with_independent_verification",
        ),
        (ChildWorkflowSelector::WorkflowRef(name), Some(found))
            if name == DEEPER_CHILD_TEMPLATE && found.is_deeper() =>
        {
            (
                Some(name.clone()),
                match found {
                    ChildWorkflowTrigger::DependentPhases => "workflow_ref_dependent_phases",
                    ChildWorkflowTrigger::FanOut => "workflow_ref_fan_out",
                    ChildWorkflowTrigger::DistinctReview => "workflow_ref_distinct_review",
                    ChildWorkflowTrigger::CrashRecovery => "workflow_ref_crash_recovery",
                    ChildWorkflowTrigger::MutationWithIndependentVerification => unreachable!(),
                },
            )
        }
        (ChildWorkflowSelector::WorkflowRef(name), Some(_))
            if matches!(
                name.as_str(),
                IMPLEMENT_VERIFY_CHILD_TEMPLATE | DEEPER_CHILD_TEMPLATE
            ) =>
        {
            (None, "workflow_ref_trigger_mismatch")
        }
        (ChildWorkflowSelector::WorkflowRef(name), Some(_)) if registered_workflow_ref => {
            (Some(name.clone()), "registered_loom_workflow_ref")
        }
        (ChildWorkflowSelector::WorkflowRef(_), Some(_)) => {
            (None, "workflow_ref_not_registered_child_template")
        }
        (ChildWorkflowSelector::WorkflowRef(_), None) => (None, "missing_workflow_trigger"),
    };
    ChildWorkflowDecision {
        requested,
        trigger,
        workflow_author: template.is_some()
            && workflow_author_requested
            && trigger.is_some_and(ChildWorkflowTrigger::is_deeper),
        template,
        reason: reason.to_owned(),
    }
}

/// Exact parent graph obligation coordinates captured when the spawn tool is
/// accepted. They are immutable even if that graph later retries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParentGraphAttempt {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
}

/// The simple cache identity. `gate_structure` describes gate/authority
/// shape only; dependency edges and the full DAG are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChildTemplateCacheKey {
    pub task_shape: String,
    pub effective_grant_digest: String,
    pub gate_structure: String,
}

impl ChildTemplateCacheKey {
    #[must_use]
    pub fn digest(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        blake3::hash(&bytes).to_hex().to_string()
    }
}

/// New parent-journal boundary fact. This is an attachment/contract, not an
/// executable edge: child and parent DAGs continue to reduce independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildGraphAttached {
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub parent_tool_item_id: ItemId,
    pub parent_attempt: ParentGraphAttempt,
    pub parent_slot: String,
    pub parent_authority: EvidenceAuthority,
    pub child_session_id: SessionId,
    pub child_run_id: RunId,
    pub child_graph_id: GraphId,
    pub workflow: ChildWorkflowSelector,
    pub template: String,
    pub digest: String,
    pub gate_reason: String,
    pub cache_key: ChildTemplateCacheKey,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_hit: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub workflow_author: bool,
}

/// One successful equivalent child workflow, keyed by an exact distinct
/// parent attempt. These append-only observations are the cache authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildTemplateObserved {
    pub cache_key: ChildTemplateCacheKey,
    pub parent_attempt: ParentGraphAttempt,
    pub collapse_evidence_seq: u64,
    pub child_contract: ChildContractRef,
    pub template: GraphTemplateSpec,
    pub digest: String,
}

/// Audit fact emitted exactly when an observation first reaches the
/// three-distinct-attempt promotion threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildTemplatePromoted {
    pub cache_key: ChildTemplateCacheKey,
    pub template: String,
    pub digest: String,
    pub distinct_parent_attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphBlockReason {
    RoundsExhausted,
    NoProgress,
    HumanHold,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphBlocked {
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub reason: GraphBlockReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCompleted {
    pub graph_id: GraphId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphAbandoned {
    pub graph_id: GraphId,
    pub why: String,
}

/// Durable claim that one provider run tried to finalize while the active
/// graph still had obligations. A repeated `(run_id, state_digest)` is the
/// fail-closed crash/replay coordinate; a changed digest proves genuine graph
/// progress. `(graph_id, run_id)` bounds the one automatic reminder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFinalizationDeferred {
    pub graph_id: GraphId,
    pub run_id: RunId,
    pub state_digest: String,
    /// Logical provider requests spent through this clean continuation
    /// checkpoint. Recovery restores it so restart cannot reset the loop cap.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub provider_requests_consumed: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet_nodes: Vec<GraphNodeName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphPhase {
    Active,
    Blocked,
    Completed,
    Abandoned,
    Superseded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidenceTally {
    pub green: u32,
    pub red: u32,
    /// Greens in the current attempt after the newest red. With no item key
    /// in M1, a red conservatively invalidates the earlier green run.
    pub effective_green: u32,
    /// Either zero or one in M1: a later green is explicit re-evidence and
    /// clears the immediately preceding red from the standing frontier.
    pub standing_red: u32,
}

/// Current replacement frontier for one declared evidence slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidenceSlotStatus {
    pub id: String,
    pub authority: EvidenceAuthority,
    pub subject_selector: SubjectSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<EvidenceVerdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<GraphEvidenceSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeStatus {
    pub node: GraphNodeName,
    /// Present for M2b pins so consumers can render/behave by properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GraphGateKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<GraphExecutorShape>,
    pub attempts_opened: u32,
    pub current_attempt: Option<u32>,
    pub evidence: GraphEvidenceTally,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_slots: Vec<GraphEvidenceSlotStatus>,
    pub satisfied: bool,
}

impl GraphNodeStatus {
    #[must_use]
    pub fn slot_statuses(&self) -> &[GraphEvidenceSlotStatus] {
        &self.evidence_slots
    }

    fn clear_evidence_frontier(&mut self) {
        self.evidence = GraphEvidenceTally::default();
        for slot in &mut self.evidence_slots {
            slot.verdict = None;
            slot.fingerprint = None;
            slot.subject_digest = None;
            slot.source = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStatus {
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub template_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_node: Option<GraphNodeName>,
    pub phase: GraphPhase,
    pub current_node: Option<GraphNodeName>,
    /// Every open obligation in template declaration order. Empty on legacy
    /// reductions, where `current_node` remains the exact old projection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ready_nodes: Vec<GraphNodeName>,
    pub attempt: u32,
    pub nodes: Vec<GraphNodeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GraphBlockReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_menu: Option<MenuId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_menus: Vec<MenuId>,
    /// Present only when this graph is the aggregate owner of an M2d todo
    /// run-set. Legacy graph statuses omit the field byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_set: Option<GraphRunSetStatus>,
}

impl GraphStatus {
    #[must_use]
    pub fn accepts_evidence(&self, node: &GraphNodeName) -> bool {
        if self.phase != GraphPhase::Active || !self.node_is_ready(node) {
            return false;
        }
        self.nodes
            .iter()
            .find(|status| &status.node == node)
            .and_then(|status| status.gate.as_ref())
            .is_some_and(|gate| !matches!(gate, GraphGateKind::HumanConfirm))
    }

    #[must_use]
    pub fn is_unfinished(&self) -> bool {
        matches!(self.phase, GraphPhase::Active | GraphPhase::Blocked)
    }

    /// Compact volatile provider context. This is never a durable fact.
    #[must_use]
    pub fn graph_brief(&self) -> Option<String> {
        if !self.is_unfinished() {
            return None;
        }
        if let Some(run_set) = &self.run_set {
            let children = run_set
                .children
                .iter()
                .map(|child| {
                    let stage = child
                        .current_node
                        .as_ref()
                        .map_or_else(|| format!("{:?}", child.phase), ToString::to_string);
                    format!("{}={}@{}", child.todo_id, child.graph_id, stage)
                })
                .collect::<Vec<_>>()
                .join(",");
            let line = format!(
                "GraphBrief: todo run-set {} {}/{} terminal; children [{}]. Record evidence against the child graph_id and its open stage.",
                run_set.run_set_id, run_set.terminal_children, run_set.required_children, children,
            );
            return Some(bound_graph_brief(line));
        }
        let node = self.current_node.as_ref()?;
        let node_status = self.nodes.iter().find(|status| &status.node == node)?;
        let gate_kind = node_status.gate.as_ref();
        let gate = match gate_kind {
            Some(GraphGateKind::CommandGreen) => "command-green",
            Some(GraphGateKind::AllOfN { .. }) => "all-of-n",
            Some(GraphGateKind::HumanConfirm) => "human-confirm",
            // Exact display fallback for pre-M2b statuses, which carried no
            // gate properties on the wire. New control flow never uses it.
            None if node.as_str() == "VERIFY" => "all-of-3",
            None if node.as_str() == "SHIP" => "human-confirm",
            None => "command-green",
        };
        let expectation = match (self.phase, gate_kind) {
            (GraphPhase::Blocked, _) => "re-pin or abandon",
            (_, Some(GraphGateKind::HumanConfirm)) => "await explicit human confirm",
            (_, Some(GraphGateKind::AllOfN { .. })) => "record all declared evidence slots",
            (_, Some(GraphGateKind::CommandGreen)) => "record green command evidence",
            (_, None) if node.as_str() == "SHIP" => "await explicit human confirm",
            (_, None) if node.as_str() == "VERIFY" => "record 3 green VERIFY results",
            (_, None) => "record BUILD evidence",
        };
        let ready = if self.ready_nodes.is_empty() {
            node.label().to_owned()
        } else {
            self.ready_nodes
                .iter()
                .map(GraphNodeName::label)
                .collect::<Vec<_>>()
                .join(",")
        };
        let line = format!(
            "GraphBrief: {} attempt {}/{}; graph_id={}; ready={}; gate {}; evidence {} green/{} red ({} effective); next: {}.",
            node.label(),
            node_status.current_attempt.unwrap_or(self.attempt),
            GRAPH_MAX_ATTEMPTS,
            self.graph_id,
            ready,
            gate,
            node_status.evidence.green,
            node_status.evidence.red,
            node_status.evidence.effective_green,
            expectation,
        );
        Some(bound_graph_brief(line))
    }

    #[must_use]
    pub fn node_is_ready(&self, node: &GraphNodeName) -> bool {
        if self.ready_nodes.is_empty() {
            self.current_node.as_ref() == Some(node)
        } else {
            self.ready_nodes.iter().any(|ready| ready == node)
        }
    }

    fn node_mut(&mut self, node: &GraphNodeName) -> Option<&mut GraphNodeStatus> {
        self.nodes.iter_mut().find(|status| &status.node == node)
    }
}

fn bound_graph_brief(line: String) -> String {
    crate::context::elide_text_head_tail(&line, GRAPH_BRIEF_MAX_BYTES, "graph_brief")
        .map_or(line, |elided| elided.text)
}

/// Read-facing projection of one todo-bound child graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoGraphStatus {
    pub todo_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on_todo_id: Option<u32>,
    pub graph_id: GraphId,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ordinal: u32,
    pub phase: GraphPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node: Option<GraphNodeName>,
    pub attempt: u32,
}

/// Current aggregate state for one Plan list. Terminal children contribute
/// exactly one contract apiece regardless of whether they completed,
/// abandoned, or were superseded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRunSetStatus {
    pub run_set_id: GraphRunSetId,
    pub root_graph_id: GraphId,
    pub plan_item_id: ItemId,
    pub plan_event_id: EventId,
    pub required_children: u32,
    pub terminal_children: u32,
    #[serde(default)]
    pub children: Vec<TodoGraphStatus>,
}

impl GraphRunSetStatus {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.terminal_children == self.required_children
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphReduction {
    pub status: Option<GraphStatus>,
    pub evidence: Vec<EvidenceRecorded>,
    /// Durable finalization attempts retained for restart-safe guardrail
    /// idempotence. This is a projection only; the journal remains authority.
    pub finalization_deferrals: Vec<GraphFinalizationDeferred>,
    /// Every guardrail confirmation ever opened for this graph. Pending state
    /// remains represented by `GraphStatus::pending_menus`.
    pub finalization_menus: Vec<GraphFinalizationMenu>,
    /// The immutable executable specs stamped by the latest GraphPinned
    /// fact. Kept beside the wire-facing status so resumed old instances are
    /// reduced with their own gates and bounds, not current binary defaults.
    pub template_nodes: Vec<GraphNodeSpec>,
}

impl GraphReduction {
    fn status_for_graph_mut(&mut self, graph_id: &GraphId) -> Option<&mut GraphStatus> {
        self.status
            .as_mut()
            .filter(|status| status.graph_id == *graph_id)
    }
}

/// Full session graph forest. Superseded instances remain queryable while the
/// active-root pointer selects the instance used by legacy status callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphReductions {
    pub active_root: Option<GraphId>,
    pub by_graph: HashMap<GraphId, GraphReduction>,
    pub active_run_set: Option<GraphRunSetId>,
    pub run_sets: HashMap<GraphRunSetId, GraphRunSetStatus>,
}

/// Coordinates retained from a durable `GraphAbandonConfirm` menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphFinalizationMenu {
    pub menu_id: MenuId,
    pub run_id: RunId,
    pub state_digest: String,
}

/// How one node-attempt interval ended in the committed journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphNodeAttemptOutcome {
    Open,
    Satisfied,
    Retried,
    Blocked,
    Completed,
    Abandoned,
    Superseded,
}

/// Rebuildable wall-clock interval for one opened node in one graph epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeAttemptRow {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
    pub opened_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_ms: Option<u64>,
    pub wall_ms: u64,
    pub outcome: GraphNodeAttemptOutcome,
}

/// Rebuildable telemetry for one immutable pinned graph instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRunRow {
    pub session_id: SessionId,
    pub graph_id: GraphId,
    pub template: String,
    pub template_version: u32,
    pub digest: String,
    pub phase: GraphPhase,
    pub opened_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at_ms: Option<u64>,
    pub wall_elapsed_ms: u64,
    pub critical_path_elapsed_ms: u64,
    pub declared_nodes: u32,
    pub node_attempts: u32,
    pub mis_gate_count: u32,
    pub override_count: u32,
    /// M2d ownership metadata. Omitted for every legacy/single graph row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<GraphRunScope>,
}

/// Stage histogram entry for a todo run-set aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStageCount {
    pub phase: GraphPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<GraphNodeName>,
    pub count: u32,
}

/// Distinguishes ordinary graph rows, todo children, and the synthetic
/// aggregate row without changing legacy row bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphRunScope {
    TodoChild {
        run_set_id: GraphRunSetId,
        plan_item_id: ItemId,
        todo_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        depends_on_todo_id: Option<u32>,
    },
    RunSetAggregate {
        run_set_id: GraphRunSetId,
        plan_item_id: ItemId,
        completed_children: u32,
        required_children: u32,
        #[serde(default)]
        stage_counts: Vec<GraphStageCount>,
    },
}

impl GraphRunScope {
    fn is_run_set_aggregate(&self) -> bool {
        matches!(self, Self::RunSetAggregate { .. })
    }
}

/// Per-node aggregate nested under a template rollup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeRollup {
    pub node: GraphNodeName,
    pub attempts: u64,
    pub wall_ms: u64,
}

/// Stable, integer-only profile aggregate for one immutable template digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphTemplateRollup {
    pub template: String,
    pub template_version: u32,
    pub digest: String,
    pub runs: u64,
    pub active: u64,
    pub blocked: u64,
    pub completed: u64,
    pub abandoned: u64,
    pub superseded: u64,
    pub completion_rate_basis_points: u32,
    pub abandon_rate_basis_points: u32,
    pub mis_gate_count: u64,
    pub override_count: u64,
    pub node_attempts: u64,
    pub declared_nodes: u64,
    pub attempts_per_node_millis: u64,
    pub node_wall_ms: u64,
    pub critical_path_elapsed_ms: u64,
    pub nodes: Vec<GraphNodeRollup>,
}

/// Session-scoped tool-selection aggregate rebuilt exclusively from tool-call
/// item lifecycles and their correlated terminal results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSelectionRow {
    pub tool_name: String,
    pub total_calls: u64,
    pub error_count: u64,
    pub error_rate_basis_points: u32,
    pub redundant_call_count: u64,
}

/// Complete deterministic telemetry projection over a bounded journal input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphTelemetryProjection {
    #[serde(default)]
    pub graph_runs: Vec<GraphRunRow>,
    #[serde(default)]
    pub graph_node_attempts: Vec<GraphNodeAttemptRow>,
    #[serde(default)]
    pub graph_template_rollups: Vec<GraphTemplateRollup>,
    #[serde(default)]
    pub tool_selection: Vec<ToolSelectionRow>,
}

/// Signal fields safe for inspection. Digests and durable coordinates are
/// exposed; transcript/output bytes are never carried here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSignalProvenance {
    pub command_arg_digest: String,
    pub exit_code: Option<i32>,
    pub transcript_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
    pub subject_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphWorkspaceMutationProvenance {
    pub effect_id: EffectId,
    pub mutation_digest: String,
    pub workspace_revision: WorkspaceRevision,
    pub subject_digest: String,
}

/// One bounded evidence-log row returned by `graph.inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEvidenceProvenanceRow {
    pub seq: u64,
    pub committed_at_ms: u64,
    pub graph_id: GraphId,
    pub node: GraphNodeName,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub authority: EvidenceAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_selector: Option<SubjectSelector>,
    pub verdict: EvidenceVerdict,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    pub source: GraphEvidenceSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<GraphSignalProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation: Option<GraphWorkspaceMutationProvenance>,
}

/// Bounded read model returned by the feature-negotiated inspect RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphInspectSnapshot {
    pub through_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<GraphStatus>,
    #[serde(default)]
    pub runs: Vec<GraphRunRow>,
    #[serde(default)]
    pub template_rollups: Vec<GraphTemplateRollup>,
    #[serde(default)]
    pub evidence: Vec<GraphEvidenceProvenanceRow>,
    #[serde(default)]
    pub tool_selection: Vec<ToolSelectionRow>,
}

impl GraphReductions {
    #[must_use]
    pub fn active(&self) -> Option<&GraphReduction> {
        self.active_root
            .as_ref()
            .and_then(|graph_id| self.by_graph.get(graph_id))
    }

    #[must_use]
    pub fn graph(&self, graph_id: &GraphId) -> Option<&GraphReduction> {
        self.by_graph.get(graph_id)
    }
}

/// Compatibility projection of the active root from [`reduce_graphs`].
#[must_use]
pub fn reduce_graph(envelopes: &[RawEnvelope]) -> GraphReduction {
    reduce_graphs(envelopes)
        .active()
        .cloned()
        .unwrap_or_default()
}

/// Reduces every immutable graph instance in a session journal. Unknown
/// payloads remain tolerated and old pins retain their exact status shape.
#[must_use]
pub fn reduce_graphs(envelopes: &[RawEnvelope]) -> GraphReductions {
    let mut reductions = GraphReductions::default();
    for envelope in envelopes {
        reductions.apply_envelope_unprojected(envelope);
    }
    refresh_run_set_projections(&mut reductions);
    reductions
}

impl GraphReductions {
    /// Incrementally folds one journal envelope into this session projection.
    /// Run-set children and their aggregate root are coherent on return. This
    /// is exposed so same-head observers do not need to replay an unchanged
    /// prefix; batch reducers use the unprojected fold and reconcile once.
    pub fn apply_envelope(&mut self, envelope: &RawEnvelope) {
        if self.apply_envelope_unprojected(envelope) {
            refresh_run_set_projections(self);
        }
    }

    /// Applies graph facts without rebuilding derived run-set aggregates.
    /// Callers folding a batch must invoke `refresh_run_set_projections` once
    /// after its final envelope.
    fn apply_envelope_unprojected(&mut self, envelope: &RawEnvelope) -> bool {
        let Some(payload) = graph_reduction_payload(&envelope.payload) else {
            return false;
        };
        self.apply_payload(payload);
        true
    }

    fn apply_payload(&mut self, payload: EventPayload) {
        match payload {
            EventPayload::GraphPinned(pinned) => {
                let GraphPinned {
                    graph_id,
                    template,
                    digest,
                    template_version,
                    start_node,
                    nodes: template_nodes,
                } = pinned;
                let m2b = template_version > 0 && start_node.is_some();
                let status = GraphStatus {
                    graph_id: graph_id.clone(),
                    template,
                    digest,
                    template_version,
                    start_node,
                    phase: GraphPhase::Active,
                    current_node: None,
                    ready_nodes: Vec::new(),
                    attempt: 0,
                    nodes: template_nodes
                        .iter()
                        .map(|spec| GraphNodeStatus {
                            node: spec.name.clone(),
                            gate: m2b.then(|| spec.gate.clone()),
                            executor: m2b.then_some(spec.executor),
                            attempts_opened: 0,
                            current_attempt: None,
                            evidence: GraphEvidenceTally::default(),
                            evidence_slots: spec
                                .verify_slots
                                .iter()
                                .map(|slot| GraphEvidenceSlotStatus {
                                    id: slot.id.clone(),
                                    authority: slot.authority,
                                    subject_selector: slot.subject_selector,
                                    verdict: None,
                                    fingerprint: None,
                                    subject_digest: None,
                                    source: None,
                                })
                                .collect(),
                            satisfied: false,
                        })
                        .collect(),
                    blocked_reason: None,
                    pending_menu: None,
                    pending_menus: Vec::new(),
                    run_set: None,
                };
                self.by_graph.insert(
                    graph_id.clone(),
                    GraphReduction {
                        status: Some(status),
                        evidence: Vec::new(),
                        finalization_deferrals: Vec::new(),
                        finalization_menus: Vec::new(),
                        template_nodes,
                    },
                );
                let attached_child = self.run_sets.values().any(|run_set| {
                    run_set
                        .children
                        .iter()
                        .any(|child| child.graph_id == graph_id)
                });
                if !attached_child {
                    self.active_root = Some(graph_id);
                }
            }
            EventPayload::GraphRunSetOpened(opened) => {
                if self.run_sets.contains_key(&opened.run_set_id)
                    || self.graph(&opened.root_graph_id).is_none()
                {
                    return;
                }
                self.active_run_set = Some(opened.run_set_id.clone());
                self.run_sets.insert(
                    opened.run_set_id.clone(),
                    GraphRunSetStatus {
                        run_set_id: opened.run_set_id,
                        root_graph_id: opened.root_graph_id,
                        plan_item_id: opened.plan_item_id,
                        plan_event_id: opened.plan_event_id,
                        required_children: opened.required_children,
                        terminal_children: 0,
                        children: Vec::new(),
                    },
                );
            }
            EventPayload::TodoGraphAttached(attached) => {
                let child_graph_already_attached = self.run_sets.values().any(|candidate| {
                    candidate
                        .children
                        .iter()
                        .any(|child| child.graph_id == attached.child_graph_id)
                });
                let Some(run_set) = self.run_sets.get_mut(&attached.run_set_id) else {
                    return;
                };
                if run_set.plan_item_id != attached.plan_item_id
                    || run_set.children.len()
                        >= usize::try_from(run_set.required_children).unwrap_or(usize::MAX)
                    || run_set
                        .children
                        .iter()
                        .any(|child| child.todo_id == attached.todo_id)
                    || child_graph_already_attached
                {
                    return;
                }
                run_set.children.push(TodoGraphStatus {
                    todo_id: attached.todo_id,
                    depends_on_todo_id: attached.depends_on_todo_id,
                    graph_id: attached.child_graph_id,
                    ordinal: attached.ordinal,
                    phase: GraphPhase::Active,
                    current_node: None,
                    attempt: 0,
                });
                run_set
                    .children
                    .sort_by_key(|child| (child.ordinal, child.todo_id));
            }
            EventPayload::GraphAttemptOpened(opened) => {
                let Some(reduction) = self.by_graph.get_mut(&opened.graph_id) else {
                    return;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&opened.graph_id) else {
                    return;
                };
                if status.phase != GraphPhase::Active {
                    return;
                }
                let reopening = status
                    .nodes
                    .iter()
                    .find(|node| node.node == opened.node)
                    .is_some_and(|node| node.attempts_opened > 0);
                status.attempt = status.attempt.max(opened.attempt);
                let start_node = status.start_node.clone().unwrap_or_else(build_node);
                if opened.node == start_node {
                    // START dominates every valid modern DAG, so its targeted
                    // forward slice is the whole graph. Keep the explicit
                    // whole-graph branch for legacy pins whose unstamped
                    // dependency lists cannot reconstruct that same slice.
                    for node in &mut status.nodes {
                        node.current_attempt = None;
                        node.clear_evidence_frontier();
                        node.satisfied = false;
                    }
                    status.ready_nodes.clear();
                } else if reopening {
                    // A targeted retry invalidates exactly the reopened node
                    // and its dependency descendants. Independent fork
                    // siblings keep their green/partial frontiers and may
                    // legitimately retain an older node-local epoch.
                    let invalidated = graph_descendants_inclusive(&template_nodes, &opened.node);
                    for node in &mut status.nodes {
                        if invalidated.contains(&node.node) {
                            node.current_attempt = None;
                            node.clear_evidence_frontier();
                            node.satisfied = false;
                        }
                    }
                    status
                        .ready_nodes
                        .retain(|node| !invalidated.contains(node));
                    refresh_current_node(status, &template_nodes);
                }
                if let Some(node) = status.node_mut(&opened.node) {
                    node.attempts_opened = node.attempts_opened.saturating_add(1);
                    node.current_attempt = Some(opened.attempt);
                    node.clear_evidence_frontier();
                    node.satisfied = false;
                }
                if status.template_version == 0 {
                    status.current_node = Some(opened.node);
                } else {
                    push_ready_in_template_order(status, &template_nodes, opened.node);
                }
            }
            EventPayload::EvidenceRecorded(recorded) => {
                let Some(reduction) = self.by_graph.get_mut(&recorded.graph_id) else {
                    return;
                };
                let Some(status) = reduction.status.as_mut() else {
                    return;
                };
                if status.phase != GraphPhase::Active
                    || !status.node_is_ready(&recorded.node)
                    || status
                        .nodes
                        .iter()
                        .find(|node| node.node == recorded.node)
                        .is_none_or(|node| node.current_attempt != Some(recorded.attempt))
                {
                    return;
                }
                // Computer observations are durable provenance attached to
                // the active node, not gate testimony. A screenshot must
                // never complete BUILD, replace a declared slot, or consume
                // the evidence-round budget merely because capture worked.
                let gate_eligible = !matches!(
                    &recorded.source,
                    GraphEvidenceSource::ComputerObservation { .. }
                );
                if gate_eligible && let Some(node) = status.node_mut(&recorded.node) {
                    if node.evidence_slots.is_empty() {
                        // Exact M1 compatibility branch for legacy pinned
                        // instances that predate evidence slot declarations.
                        match recorded.verdict {
                            EvidenceVerdict::Green => {
                                node.evidence.green = node.evidence.green.saturating_add(1);
                                node.evidence.effective_green =
                                    node.evidence.effective_green.saturating_add(1);
                                node.evidence.standing_red = 0;
                            }
                            EvidenceVerdict::Red => {
                                node.evidence.red = node.evidence.red.saturating_add(1);
                                node.evidence.effective_green = 0;
                                node.evidence.standing_red = 1;
                            }
                        }
                    } else if let Some(slot) = recorded.slot.as_deref().and_then(|slot_id| {
                        node.evidence_slots
                            .iter_mut()
                            .find(|slot| slot.id == slot_id)
                    }) {
                        match recorded.verdict {
                            EvidenceVerdict::Green => {
                                node.evidence.green = node.evidence.green.saturating_add(1);
                            }
                            EvidenceVerdict::Red => {
                                node.evidence.red = node.evidence.red.saturating_add(1);
                            }
                        }
                        slot.verdict = Some(recorded.verdict);
                        slot.fingerprint = Some(recorded.fingerprint.clone());
                        slot.subject_digest = recorded.subject_digest.clone();
                        slot.source = Some(recorded.source.clone());
                        node.evidence.effective_green = u32::try_from(
                            node.evidence_slots
                                .iter()
                                .filter(|slot| slot.verdict == Some(EvidenceVerdict::Green))
                                .count(),
                        )
                        .unwrap_or(u32::MAX);
                        node.evidence.standing_red = u32::try_from(
                            node.evidence_slots
                                .iter()
                                .filter(|slot| slot.verdict == Some(EvidenceVerdict::Red))
                                .count(),
                        )
                        .unwrap_or(u32::MAX);
                    }
                }
                reduction.evidence.push(recorded);
            }
            EventPayload::GraphGateSatisfied(satisfied) => {
                let Some(reduction) = self.by_graph.get_mut(&satisfied.graph_id) else {
                    return;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&satisfied.graph_id) else {
                    return;
                };
                if status.phase != GraphPhase::Active
                    || !status.node_is_ready(&satisfied.node)
                    || status
                        .nodes
                        .iter()
                        .find(|node| node.node == satisfied.node)
                        .is_none_or(|node| node.current_attempt != Some(satisfied.attempt))
                {
                    return;
                }
                if let Some(node) = status.node_mut(&satisfied.node) {
                    node.satisfied = true;
                }
                if status.template_version > 0 {
                    status.ready_nodes.retain(|node| node != &satisfied.node);
                    refresh_current_node(status, &template_nodes);
                }
            }
            EventPayload::GraphAdvanced(advanced) => {
                if let Some(status) = self
                    .by_graph
                    .get_mut(&advanced.graph_id)
                    .and_then(|reduction| reduction.status_for_graph_mut(&advanced.graph_id))
                    && status.template_version == 0
                {
                    status.current_node = Some(advanced.to_node);
                }
            }
            EventPayload::GraphNodeReadied(readied) => {
                let Some(reduction) = self.by_graph.get_mut(&readied.graph_id) else {
                    return;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&readied.graph_id) else {
                    return;
                };
                if status.phase == GraphPhase::Active && status.attempt == readied.attempt {
                    push_ready_in_template_order(status, &template_nodes, readied.node);
                }
            }
            EventPayload::GraphBlocked(blocked) => {
                if let Some(status) = self
                    .by_graph
                    .get_mut(&blocked.graph_id)
                    .and_then(|reduction| reduction.status_for_graph_mut(&blocked.graph_id))
                {
                    status.phase = GraphPhase::Blocked;
                    status.current_node = Some(blocked.node);
                    status.ready_nodes.clear();
                    status.blocked_reason = Some(blocked.reason);
                    status.pending_menu = None;
                    status.pending_menus.clear();
                }
            }
            EventPayload::GraphCompleted(completed) => {
                if let Some(status) = self
                    .by_graph
                    .get_mut(&completed.graph_id)
                    .and_then(|reduction| reduction.status_for_graph_mut(&completed.graph_id))
                {
                    status.phase = GraphPhase::Completed;
                    if status.template_version > 0 {
                        status.current_node = None;
                    }
                    status.ready_nodes.clear();
                    status.pending_menu = None;
                    status.pending_menus.clear();
                }
            }
            EventPayload::GraphAbandoned(abandoned) => {
                if let Some(status) = self
                    .by_graph
                    .get_mut(&abandoned.graph_id)
                    .and_then(|reduction| reduction.status_for_graph_mut(&abandoned.graph_id))
                {
                    status.phase = GraphPhase::Abandoned;
                    status.ready_nodes.clear();
                    status.pending_menu = None;
                    status.pending_menus.clear();
                }
            }
            EventPayload::GraphFinalizationDeferred(deferred) => {
                if let Some(reduction) = self.by_graph.get_mut(&deferred.graph_id)
                    && !reduction.finalization_deferrals.iter().any(|prior| {
                        prior.run_id == deferred.run_id
                            && prior.state_digest == deferred.state_digest
                    })
                {
                    reduction.finalization_deferrals.push(deferred);
                }
            }
            EventPayload::GraphSuperseded(superseded) => {
                if let Some(status) = self
                    .by_graph
                    .get_mut(&superseded.old)
                    .and_then(|reduction| reduction.status_for_graph_mut(&superseded.old))
                {
                    status.phase = GraphPhase::Superseded;
                    status.current_node = None;
                    status.ready_nodes.clear();
                    status.pending_menu = None;
                    status.pending_menus.clear();
                }
                let child_supersession = self.run_sets.values().any(|run_set| {
                    run_set
                        .children
                        .iter()
                        .any(|child| child.graph_id == superseded.old)
                });
                if !child_supersession {
                    self.active_root = Some(superseded.new);
                }
            }
            EventPayload::MenuOpened(menu) => {
                let finalization_menu = matches!(
                    &menu.kind,
                    crate::menu::MenuKind::GraphAbandonConfirm { .. }
                );
                let graph_id = match &menu.kind {
                    crate::menu::MenuKind::GraphHumanConfirm { graph_id, .. }
                    | crate::menu::MenuKind::GraphAbandonConfirm { graph_id, .. } => Some(graph_id),
                    _ => None,
                };
                if let Some(graph_id) = graph_id
                    && let Some(status) = self
                        .by_graph
                        .get_mut(graph_id)
                        .and_then(|reduction| reduction.status_for_graph_mut(graph_id))
                {
                    status.pending_menu.get_or_insert_with(|| menu.id.clone());
                    if (status.template_version > 0 || finalization_menu)
                        && !status.pending_menus.iter().any(|id| id == &menu.id)
                    {
                        status.pending_menus.push(menu.id.clone());
                    }
                }
                if let crate::menu::MenuKind::GraphAbandonConfirm {
                    graph_id,
                    run_id,
                    state_digest,
                } = menu.kind
                    && let Some(reduction) = self.by_graph.get_mut(&graph_id)
                {
                    reduction.finalization_menus.push(GraphFinalizationMenu {
                        menu_id: menu.id,
                        run_id,
                        state_digest,
                    });
                }
            }
            EventPayload::MenuAnswered(crate::menu::MenuAnswer { menu, .. })
            | EventPayload::MenuClosed { menu, .. } => {
                for reduction in self.by_graph.values_mut() {
                    if let Some(status) = reduction.status.as_mut() {
                        status.pending_menus.retain(|pending| pending != &menu);
                        if status.pending_menu.as_ref() == Some(&menu) {
                            status.pending_menu = status.pending_menus.first().cloned();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn refresh_run_set_projections(reductions: &mut GraphReductions) {
    for run_set in reductions.run_sets.values_mut() {
        for child in &mut run_set.children {
            let Some(status) = reductions
                .by_graph
                .get(&child.graph_id)
                .and_then(|reduction| reduction.status.as_ref())
            else {
                continue;
            };
            child.phase = status.phase;
            child.current_node = status.current_node.clone();
            child.attempt = status.attempt;
        }
        run_set.terminal_children = u32::try_from(
            run_set
                .children
                .iter()
                .filter(|child| graph_phase_is_terminal(child.phase))
                .count(),
        )
        .unwrap_or(u32::MAX)
        .min(run_set.required_children);
    }

    let owned = reductions
        .active_run_set
        .as_ref()
        .and_then(|run_set_id| reductions.run_sets.get(run_set_id))
        .cloned();
    let Some(run_set) = owned else {
        return;
    };
    let Some(root) = reductions
        .by_graph
        .get_mut(&run_set.root_graph_id)
        .and_then(|reduction| reduction.status.as_mut())
    else {
        return;
    };
    root.run_set = Some(run_set.clone());
    if !matches!(root.phase, GraphPhase::Abandoned | GraphPhase::Superseded) {
        root.phase = if run_set.is_complete() {
            GraphPhase::Completed
        } else {
            GraphPhase::Active
        };
        root.current_node = None;
        root.ready_nodes.clear();
        root.pending_menu = None;
        root.pending_menus.clear();
    }
}

fn graph_phase_is_terminal(phase: GraphPhase) -> bool {
    matches!(
        phase,
        GraphPhase::Completed | GraphPhase::Abandoned | GraphPhase::Superseded
    )
}

fn push_ready_in_template_order(
    status: &mut GraphStatus,
    template_nodes: &[GraphNodeSpec],
    node: GraphNodeName,
) {
    if !status.ready_nodes.iter().any(|ready| ready == &node) {
        status.ready_nodes.push(node);
    }
    refresh_current_node(status, template_nodes);
}

fn refresh_current_node(status: &mut GraphStatus, template_nodes: &[GraphNodeSpec]) {
    status.ready_nodes.sort_by_key(|ready| {
        template_nodes
            .iter()
            .position(|spec| &spec.name == ready)
            .unwrap_or(usize::MAX)
    });
    status.current_node = status.ready_nodes.first().cloned();
}

fn graph_descendants_inclusive(
    specs: &[GraphNodeSpec],
    target: &GraphNodeName,
) -> HashSet<GraphNodeName> {
    let mut descendants = HashSet::from([target.clone()]);
    let mut queue = VecDeque::from([target.clone()]);
    while let Some(node) = queue.pop_front() {
        for dependent in specs.iter().filter(|candidate| {
            candidate
                .depends_on
                .iter()
                .any(|dependency| dependency == &node)
        }) {
            if descendants.insert(dependent.name.clone()) {
                queue.push_back(dependent.name.clone());
            }
        }
    }
    descendants
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelemetryAttempt {
    node: GraphNodeName,
    attempt: u32,
    opened_at_ms: u64,
    closed_at_ms: Option<u64>,
    outcome: GraphNodeAttemptOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelemetryRun {
    session_id: SessionId,
    graph_id: GraphId,
    template: String,
    template_version: u32,
    digest: String,
    phase: GraphPhase,
    opened_at_ms: u64,
    terminal_at_ms: Option<u64>,
    last_observed_at_ms: u64,
    start_node: Option<GraphNodeName>,
    specs: Vec<GraphNodeSpec>,
    attempts: Vec<TelemetryAttempt>,
    mis_gate_count: u32,
    override_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelemetryRunSet {
    session_id: SessionId,
    run_set_id: GraphRunSetId,
    root_graph_id: GraphId,
    plan_item_id: ItemId,
    required_children: u32,
    opened_at_ms: u64,
}

impl TelemetryRun {
    fn close_open_attempts(&mut self, at_ms: u64, outcome: GraphNodeAttemptOutcome) {
        for attempt in &mut self.attempts {
            if attempt.closed_at_ms.is_none() {
                attempt.closed_at_ms = Some(at_ms);
                attempt.outcome = outcome;
            }
        }
        self.last_observed_at_ms = self.last_observed_at_ms.max(at_ms);
    }

    fn close_attempt(
        &mut self,
        node: &GraphNodeName,
        epoch: u32,
        at_ms: u64,
        outcome: GraphNodeAttemptOutcome,
    ) {
        if let Some(attempt) = self.attempts.iter_mut().rev().find(|attempt| {
            attempt.node == *node && attempt.attempt == epoch && attempt.closed_at_ms.is_none()
        }) {
            attempt.closed_at_ms = Some(at_ms);
            attempt.outcome = outcome;
        }
        self.last_observed_at_ms = self.last_observed_at_ms.max(at_ms);
    }

    fn close_invalidated_attempts(&mut self, invalidated: &HashSet<GraphNodeName>, at_ms: u64) {
        for attempt in &mut self.attempts {
            if attempt.closed_at_ms.is_none() && invalidated.contains(&attempt.node) {
                attempt.closed_at_ms = Some(at_ms);
                attempt.outcome = GraphNodeAttemptOutcome::Retried;
            }
        }
        self.last_observed_at_ms = self.last_observed_at_ms.max(at_ms);
    }
}

/// Durable continuation of the exact from-scratch telemetry fold. Persisted
/// instances are an optimization only: replaying the indexed journal through
/// [`Self::apply`] reconstructs byte-identical state and projection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphTelemetryAccumulator {
    runs: HashMap<(SessionId, GraphId), TelemetryRun>,
    guard_menus: HashMap<(SessionId, MenuId), (GraphId, RunId)>,
    run_sets: HashMap<(SessionId, GraphRunSetId), TelemetryRunSet>,
    todo_attachments: HashMap<(SessionId, GraphId), TodoGraphAttached>,
    reductions_by_session: HashMap<SessionId, GraphReductions>,
    tool_selection: ToolSelectionAccumulator,
}

impl GraphTelemetryAccumulator {
    /// Folds one committed envelope in journal order.
    pub fn apply(&mut self, envelope: &RawEnvelope) {
        self.reductions_by_session
            .entry(envelope.session_id.clone())
            .or_default()
            .apply_envelope_unprojected(envelope);
        self.tool_selection.apply(envelope);
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            return;
        };
        let session_id = envelope.session_id.clone();
        let at_ms = envelope.committed_at_ms;
        match payload {
            EventPayload::GraphRunSetOpened(opened) => {
                self.run_sets
                    .entry((session_id.clone(), opened.run_set_id.clone()))
                    .or_insert(TelemetryRunSet {
                        session_id,
                        run_set_id: opened.run_set_id,
                        root_graph_id: opened.root_graph_id,
                        plan_item_id: opened.plan_item_id,
                        required_children: opened.required_children,
                        opened_at_ms: at_ms,
                    });
            }
            EventPayload::TodoGraphAttached(attached) => {
                self.todo_attachments
                    .entry((session_id, attached.child_graph_id.clone()))
                    .or_insert(attached);
            }
            EventPayload::GraphPinned(pinned) => {
                let key = (session_id.clone(), pinned.graph_id.clone());
                self.runs.entry(key).or_insert_with(|| TelemetryRun {
                    session_id,
                    graph_id: pinned.graph_id,
                    template: pinned.template,
                    template_version: pinned.template_version,
                    digest: pinned.digest,
                    phase: GraphPhase::Active,
                    opened_at_ms: at_ms,
                    terminal_at_ms: None,
                    last_observed_at_ms: at_ms,
                    start_node: pinned.start_node,
                    specs: pinned.nodes,
                    attempts: Vec::new(),
                    mis_gate_count: 0,
                    override_count: 0,
                });
            }
            EventPayload::GraphAttemptOpened(opened) => {
                if let Some(run) = self.runs.get_mut(&(session_id, opened.graph_id.clone())) {
                    let reopening = run
                        .attempts
                        .iter()
                        .any(|prior| prior.node == opened.node && prior.attempt < opened.attempt);
                    if reopening {
                        let start_node = run
                            .start_node
                            .as_ref()
                            .or_else(|| run.specs.first().map(|spec| &spec.name));
                        let invalidated = if start_node == Some(&opened.node) {
                            run.specs.iter().map(|spec| spec.name.clone()).collect()
                        } else {
                            graph_descendants_inclusive(&run.specs, &opened.node)
                        };
                        run.close_invalidated_attempts(&invalidated, at_ms);
                    }
                    run.last_observed_at_ms = run.last_observed_at_ms.max(at_ms);
                    run.attempts.push(TelemetryAttempt {
                        node: opened.node,
                        attempt: opened.attempt,
                        opened_at_ms: at_ms,
                        closed_at_ms: None,
                        outcome: GraphNodeAttemptOutcome::Open,
                    });
                }
            }
            EventPayload::GraphGateSatisfied(satisfied) => {
                if let Some(run) = self.runs.get_mut(&(session_id, satisfied.graph_id.clone())) {
                    run.close_attempt(
                        &satisfied.node,
                        satisfied.attempt,
                        at_ms,
                        GraphNodeAttemptOutcome::Satisfied,
                    );
                }
            }
            EventPayload::GraphBlocked(blocked) => {
                if let Some(run) = self.runs.get_mut(&(session_id, blocked.graph_id.clone())) {
                    run.close_open_attempts(at_ms, GraphNodeAttemptOutcome::Blocked);
                    run.phase = GraphPhase::Blocked;
                }
            }
            EventPayload::GraphCompleted(completed) => {
                if let Some(run) = self.runs.get_mut(&(session_id, completed.graph_id.clone())) {
                    run.close_open_attempts(at_ms, GraphNodeAttemptOutcome::Completed);
                    run.phase = GraphPhase::Completed;
                    run.terminal_at_ms = Some(at_ms);
                }
            }
            EventPayload::GraphAbandoned(abandoned) => {
                if let Some(run) = self.runs.get_mut(&(session_id, abandoned.graph_id.clone())) {
                    run.close_open_attempts(at_ms, GraphNodeAttemptOutcome::Abandoned);
                    run.phase = GraphPhase::Abandoned;
                    run.terminal_at_ms = Some(at_ms);
                }
            }
            EventPayload::GraphSuperseded(superseded) => {
                if let Some(run) = self.runs.get_mut(&(session_id, superseded.old.clone())) {
                    run.close_open_attempts(at_ms, GraphNodeAttemptOutcome::Superseded);
                    run.phase = GraphPhase::Superseded;
                    run.terminal_at_ms = Some(at_ms);
                }
            }
            EventPayload::GraphFinalizationDeferred(deferred) => {
                if let Some(run) = self.runs.get_mut(&(session_id, deferred.graph_id.clone())) {
                    run.last_observed_at_ms = run.last_observed_at_ms.max(at_ms);
                    run.mis_gate_count = run.mis_gate_count.saturating_add(1);
                }
            }
            EventPayload::MenuOpened(menu) => {
                if let crate::menu::MenuKind::GraphAbandonConfirm {
                    graph_id, run_id, ..
                } = menu.kind
                {
                    self.guard_menus
                        .insert((session_id, menu.id), (graph_id, run_id));
                }
            }
            EventPayload::MenuAnswered(answer)
                if answer.option_key.as_deref() == Some("abandon-and-finish") =>
            {
                if let Some((graph_id, _)) = self
                    .guard_menus
                    .get(&(session_id.clone(), answer.menu))
                    .cloned()
                    && let Some(run) = self.runs.get_mut(&(session_id, graph_id))
                {
                    run.override_count = run.override_count.saturating_add(1);
                    run.last_observed_at_ms = run.last_observed_at_ms.max(at_ms);
                }
            }
            _ => {}
        }
    }

    /// Materializes the public rows from the continuation state.
    #[must_use]
    pub fn projection(&self) -> GraphTelemetryProjection {
        graph_telemetry_projection(self)
    }
}

/// Rebuilds graph adoption telemetry solely from committed journal facts.
/// The same fold drives incremental cache refresh and store reopen.
#[must_use]
pub fn reduce_graph_telemetry(envelopes: &[RawEnvelope]) -> GraphTelemetryProjection {
    let mut accumulator = GraphTelemetryAccumulator::default();
    for envelope in envelopes {
        accumulator.apply(envelope);
    }
    accumulator.projection()
}

fn graph_telemetry_projection(accumulator: &GraphTelemetryAccumulator) -> GraphTelemetryProjection {
    let runs = &accumulator.runs;
    let run_sets = &accumulator.run_sets;
    let todo_attachments = &accumulator.todo_attachments;

    let mut graph_runs = Vec::with_capacity(runs.len());
    let mut graph_node_attempts = Vec::new();
    for run in runs.values() {
        let observed_end = run.terminal_at_ms.unwrap_or(run.last_observed_at_ms);
        let attempts = run
            .attempts
            .iter()
            .map(|attempt| {
                let end = attempt.closed_at_ms.unwrap_or(observed_end);
                GraphNodeAttemptRow {
                    session_id: run.session_id.clone(),
                    graph_id: run.graph_id.clone(),
                    node: attempt.node.clone(),
                    attempt: attempt.attempt,
                    opened_at_ms: attempt.opened_at_ms,
                    closed_at_ms: attempt.closed_at_ms,
                    wall_ms: end.saturating_sub(attempt.opened_at_ms),
                    outcome: attempt.outcome,
                }
            })
            .collect::<Vec<_>>();
        let critical_path_elapsed_ms = graph_critical_path_ms(&run.specs, &attempts);
        graph_runs.push(GraphRunRow {
            session_id: run.session_id.clone(),
            graph_id: run.graph_id.clone(),
            template: run.template.clone(),
            template_version: run.template_version,
            digest: run.digest.clone(),
            phase: run.phase,
            opened_at_ms: run.opened_at_ms,
            terminal_at_ms: run.terminal_at_ms,
            wall_elapsed_ms: observed_end.saturating_sub(run.opened_at_ms),
            critical_path_elapsed_ms,
            declared_nodes: u32::try_from(run.specs.len()).unwrap_or(u32::MAX),
            node_attempts: u32::try_from(attempts.len()).unwrap_or(u32::MAX),
            mis_gate_count: run.mis_gate_count,
            override_count: run.override_count,
            scope: None,
        });
        graph_node_attempts.extend(attempts);
    }

    for row in &mut graph_runs {
        let Some(attached) = todo_attachments.get(&(row.session_id.clone(), row.graph_id.clone()))
        else {
            continue;
        };
        row.scope = Some(GraphRunScope::TodoChild {
            run_set_id: attached.run_set_id.clone(),
            plan_item_id: attached.plan_item_id.clone(),
            todo_id: attached.todo_id,
            depends_on_todo_id: attached.depends_on_todo_id,
        });
    }

    let reductions_by_session = accumulator
        .reductions_by_session
        .iter()
        .map(|(session_id, reductions)| {
            let mut reductions = reductions.clone();
            refresh_run_set_projections(&mut reductions);
            (session_id.clone(), reductions)
        })
        .collect::<HashMap<_, _>>();
    for run_set in run_sets.values() {
        let child_rows = graph_runs
            .iter()
            .filter(|row| {
                matches!(
                    &row.scope,
                    Some(GraphRunScope::TodoChild { run_set_id, .. })
                        if run_set_id == &run_set.run_set_id && row.session_id == run_set.session_id
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let completed_children = u32::try_from(
            child_rows
                .iter()
                .filter(|row| graph_phase_is_terminal(row.phase))
                .count(),
        )
        .unwrap_or(u32::MAX)
        .min(run_set.required_children);
        let complete = completed_children == run_set.required_children;
        let observed_end = child_rows
            .iter()
            .map(|row| row.opened_at_ms.saturating_add(row.wall_elapsed_ms))
            .max()
            .unwrap_or(run_set.opened_at_ms);
        let root = graph_runs
            .iter()
            .find(|row| {
                row.session_id == run_set.session_id
                    && row.graph_id == run_set.root_graph_id
                    && row.scope.is_none()
            })
            .cloned();
        let Some(root) = root else {
            continue;
        };
        let child_statuses = reductions_by_session
            .get(&run_set.session_id)
            .and_then(|reductions| reductions.run_sets.get(&run_set.run_set_id))
            .map(|status| status.children.as_slice())
            .unwrap_or_default();
        let mut stage_counts = BTreeMap::<(GraphPhase, Option<GraphNodeName>), u32>::new();
        for child in child_statuses {
            let count = stage_counts
                .entry((child.phase, child.current_node.clone()))
                .or_default();
            *count = count.saturating_add(1);
        }
        graph_runs.push(GraphRunRow {
            session_id: run_set.session_id.clone(),
            graph_id: run_set.root_graph_id.clone(),
            template: root.template.clone(),
            template_version: root.template_version,
            digest: root.digest.clone(),
            phase: if complete {
                GraphPhase::Completed
            } else {
                GraphPhase::Active
            },
            opened_at_ms: run_set.opened_at_ms,
            terminal_at_ms: complete.then_some(observed_end),
            wall_elapsed_ms: observed_end.saturating_sub(run_set.opened_at_ms),
            critical_path_elapsed_ms: child_rows
                .iter()
                .map(|row| row.critical_path_elapsed_ms)
                .max()
                .unwrap_or(0),
            declared_nodes: child_rows
                .iter()
                .fold(0_u32, |total, row| total.saturating_add(row.declared_nodes)),
            node_attempts: child_rows
                .iter()
                .fold(0_u32, |total, row| total.saturating_add(row.node_attempts)),
            mis_gate_count: child_rows
                .iter()
                .fold(0_u32, |total, row| total.saturating_add(row.mis_gate_count)),
            override_count: child_rows
                .iter()
                .fold(0_u32, |total, row| total.saturating_add(row.override_count)),
            scope: Some(GraphRunScope::RunSetAggregate {
                run_set_id: run_set.run_set_id.clone(),
                plan_item_id: run_set.plan_item_id.clone(),
                completed_children,
                required_children: run_set.required_children,
                stage_counts: stage_counts
                    .into_iter()
                    .map(|((phase, node), count)| GraphStageCount { phase, node, count })
                    .collect(),
            }),
        });
    }
    graph_runs.sort_by(|left, right| {
        left.session_id
            .as_str()
            .cmp(right.session_id.as_str())
            .then_with(|| left.opened_at_ms.cmp(&right.opened_at_ms))
            .then_with(|| left.graph_id.as_str().cmp(right.graph_id.as_str()))
    });
    graph_node_attempts.sort_by(|left, right| {
        left.session_id
            .as_str()
            .cmp(right.session_id.as_str())
            .then_with(|| left.graph_id.as_str().cmp(right.graph_id.as_str()))
            .then_with(|| left.attempt.cmp(&right.attempt))
            .then_with(|| left.opened_at_ms.cmp(&right.opened_at_ms))
            .then_with(|| left.node.cmp(&right.node))
    });
    let graph_template_rollups = graph_template_rollups(&graph_runs, &graph_node_attempts);
    let tool_selection = accumulator.tool_selection.projection();
    GraphTelemetryProjection {
        graph_runs,
        graph_node_attempts,
        graph_template_rollups,
        tool_selection,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct ToolCallLane {
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    agent_id: Option<AgentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCallTelemetry {
    lane: ToolCallLane,
    call_id: String,
    tool_name: String,
    args: serde_json::Value,
    started_seq: u64,
    completed_seq: Option<u64>,
    result_status: Option<crate::tool::ToolResultStatus>,
    candidate_assigned: bool,
    #[serde(default)]
    waiting_for_candidate: bool,
    redundant: bool,
}

/// Durable continuation state for the tool-selection projection. The
/// from-scratch reducer below drives this same fold, so serialized hot-state
/// continuation and journal replay cannot acquire separate semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSelectionAccumulator {
    calls: Vec<ToolCallTelemetry>,
    positions: HashMap<(ToolCallLane, String), usize>,
    #[serde(default)]
    lane_calls: HashMap<ToolCallLane, Vec<usize>>,
    #[serde(default)]
    pending_repairs: HashMap<ToolCallLane, Vec<usize>>,
    repair_for: HashMap<usize, Vec<usize>>,
    rollups: BTreeMap<String, (u64, u64, u64)>,
}

impl ToolSelectionAccumulator {
    pub fn apply(&mut self, envelope: &RawEnvelope) {
        let lane = ToolCallLane {
            branch_id: envelope.branch_id.clone(),
            run_id: envelope.run_id.clone(),
            agent_id: envelope.agent_id.clone(),
        };
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            return;
        };
        match payload {
            EventPayload::Item(crate::item::ItemEvent::Started {
                item:
                    crate::item::TurnItem::ToolCall {
                        call_id,
                        name,
                        args,
                        ..
                    },
                ..
            }) => {
                self.insert_call(lane, call_id, name, args, envelope.seq, None);
            }
            EventPayload::Item(crate::item::ItemEvent::Completed {
                item:
                    crate::item::TurnItem::ToolCall {
                        call_id,
                        name,
                        args,
                        ..
                    },
                ..
            }) => {
                let key = (lane.clone(), call_id.clone());
                let index = self.positions.get(&key).copied().unwrap_or_else(|| {
                    self.insert_call(
                        lane,
                        call_id,
                        name.clone(),
                        args.clone(),
                        envelope.seq,
                        Some(envelope.seq),
                    )
                });
                self.complete_call(index, name, args, envelope.seq);
            }
            EventPayload::ToolResult { call_id, result } => {
                if let Some(index) = self.positions.get(&(lane, call_id)).copied()
                    && self.calls[index].result_status.is_none()
                {
                    self.calls[index].result_status = Some(result.status);
                    if !result.status.is_completed() {
                        let row = self
                            .rollups
                            .entry(self.calls[index].tool_name.clone())
                            .or_default();
                        row.1 = row.1.saturating_add(1);
                    }
                    self.assign_repair_candidate(index);
                }
            }
            _ => {}
        }
    }

    fn insert_call(
        &mut self,
        lane: ToolCallLane,
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
        started_seq: u64,
        completed_seq: Option<u64>,
    ) -> usize {
        let key = (lane.clone(), call_id.clone());
        if let Some(index) = self.positions.get(&key).copied() {
            return index;
        }
        let index = self.calls.len();
        self.positions.insert(key, index);
        self.lane_calls.entry(lane.clone()).or_default().push(index);
        self.calls.push(ToolCallTelemetry {
            lane,
            call_id,
            tool_name: tool_name.clone(),
            args,
            started_seq,
            completed_seq,
            result_status: None,
            candidate_assigned: false,
            waiting_for_candidate: false,
            redundant: false,
        });
        let row = self.rollups.entry(tool_name).or_default();
        row.0 = row.0.saturating_add(1);
        self.attach_as_first_repair(index);
        index
    }

    fn complete_call(
        &mut self,
        index: usize,
        tool_name: String,
        args: serde_json::Value,
        completed_seq: u64,
    ) {
        let previous_name = self.calls[index].tool_name.clone();
        if previous_name != tool_name {
            let error = self.calls[index]
                .result_status
                .is_some_and(|status| !status.is_completed());
            let redundant = self.calls[index].redundant;
            if let Some(row) = self.rollups.get_mut(&previous_name) {
                row.0 = row.0.saturating_sub(1);
                row.1 = row.1.saturating_sub(u64::from(error));
                row.2 = row.2.saturating_sub(u64::from(redundant));
            }
            let row = self.rollups.entry(tool_name.clone()).or_default();
            row.0 = row.0.saturating_add(1);
            row.1 = row.1.saturating_add(u64::from(error));
            row.2 = row.2.saturating_add(u64::from(redundant));
        }
        self.calls[index].tool_name = tool_name;
        self.calls[index].args = args;
        self.calls[index].completed_seq = Some(completed_seq);
        self.evaluate_repair(index);
        self.assign_repair_candidate(index);
    }

    fn rejected(&self, index: usize) -> bool {
        matches!(
            self.calls[index].result_status,
            Some(crate::tool::ToolResultStatus::Rejected | crate::tool::ToolResultStatus::Conflict)
        ) && self.calls[index].completed_seq.is_some()
    }

    fn assign_repair_candidate(&mut self, original: usize) {
        if !self.rejected(original) || self.calls[original].candidate_assigned {
            return;
        }
        let completed_seq = self.calls[original].completed_seq.unwrap_or(u64::MAX);
        let lane = self.calls[original].lane.clone();
        let candidate = self.lane_calls.get(&lane).and_then(|calls| {
            let position =
                calls.partition_point(|index| self.calls[*index].started_seq <= completed_seq);
            calls.get(position).copied()
        });
        if let Some(candidate) = candidate {
            self.calls[original].candidate_assigned = true;
            self.calls[original].waiting_for_candidate = false;
            self.repair_for.entry(candidate).or_default().push(original);
            self.evaluate_repair(candidate);
        } else if !self.calls[original].waiting_for_candidate {
            self.calls[original].waiting_for_candidate = true;
            self.pending_repairs.entry(lane).or_default().push(original);
        }
    }

    fn attach_as_first_repair(&mut self, candidate: usize) {
        let lane = self.calls[candidate].lane.clone();
        let started_seq = self.calls[candidate].started_seq;
        let Some(originals) = self.pending_repairs.remove(&lane) else {
            return;
        };
        let mut waiting = Vec::new();
        for original in originals {
            if self.calls[original]
                .completed_seq
                .is_some_and(|seq| started_seq > seq)
            {
                self.calls[original].candidate_assigned = true;
                self.calls[original].waiting_for_candidate = false;
                self.repair_for.entry(candidate).or_default().push(original);
            } else {
                waiting.push(original);
            }
        }
        if !waiting.is_empty() {
            self.pending_repairs.insert(lane, waiting);
        }
    }

    fn evaluate_repair(&mut self, candidate: usize) {
        let Some(originals) = self.repair_for.get(&candidate).cloned() else {
            return;
        };
        if self.calls[candidate].completed_seq.is_none() {
            return;
        }
        for original in originals {
            if !self.calls[original].redundant
                && self.calls[candidate].tool_name == self.calls[original].tool_name
                && self.calls[candidate].args != self.calls[original].args
            {
                self.calls[original].redundant = true;
                let row = self
                    .rollups
                    .entry(self.calls[original].tool_name.clone())
                    .or_default();
                row.2 = row.2.saturating_add(1);
            }
        }
    }

    #[must_use]
    pub fn projection(&self) -> Vec<ToolSelectionRow> {
        let mut rows = self
            .rollups
            .iter()
            .filter(|(_, (total, _, _))| *total > 0)
            .map(
                |(tool_name, (total_calls, error_count, redundant_call_count))| {
                    let rate = error_count
                        .saturating_mul(10_000)
                        .checked_div(*total_calls)
                        .unwrap_or(0);
                    ToolSelectionRow {
                        tool_name: tool_name.clone(),
                        total_calls: *total_calls,
                        error_count: *error_count,
                        error_rate_basis_points: u32::try_from(rate).unwrap_or(u32::MAX),
                        redundant_call_count: *redundant_call_count,
                    }
                },
            )
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            right
                .total_calls
                .cmp(&left.total_calls)
                .then_with(|| left.tool_name.cmp(&right.tool_name))
        });
        rows
    }
}

/// Rebuilds the session-local tool-selection rollup from durable call/result
/// facts. A call is conservatively classified as redundant only when it ends
/// in a typed `Rejected`/`Conflict`, then the first call started after its
/// completed item in the same lane retries the same tool with different final
/// arguments. The rejected original is counted, never the repair.
#[must_use]
pub fn reduce_tool_selection(envelopes: &[RawEnvelope]) -> Vec<ToolSelectionRow> {
    let mut accumulator = ToolSelectionAccumulator::default();
    for envelope in envelopes {
        accumulator.apply(envelope);
    }
    accumulator.projection()
}

fn graph_critical_path_ms(specs: &[GraphNodeSpec], attempts: &[GraphNodeAttemptRow]) -> u64 {
    let mut epochs = BTreeMap::<u32, HashMap<GraphNodeName, u64>>::new();
    for attempt in attempts {
        epochs
            .entry(attempt.attempt)
            .or_default()
            .insert(attempt.node.clone(), attempt.wall_ms);
    }
    epochs
        .values()
        .map(|durations| {
            let mut memo = HashMap::<GraphNodeName, u64>::new();
            specs
                .iter()
                .map(|spec| graph_node_path_ms(&spec.name, specs, durations, &mut memo))
                .max()
                .unwrap_or(0)
        })
        .fold(0_u64, u64::saturating_add)
}

fn graph_node_path_ms(
    node: &GraphNodeName,
    specs: &[GraphNodeSpec],
    durations: &HashMap<GraphNodeName, u64>,
    memo: &mut HashMap<GraphNodeName, u64>,
) -> u64 {
    if let Some(elapsed) = memo.get(node) {
        return *elapsed;
    }
    let own = durations.get(node).copied().unwrap_or(0);
    let dependency = specs
        .iter()
        .find(|spec| &spec.name == node)
        .into_iter()
        .flat_map(|spec| spec.depends_on.iter())
        .map(|dependency| graph_node_path_ms(dependency, specs, durations, memo))
        .max()
        .unwrap_or(0);
    let elapsed = dependency.saturating_add(own);
    memo.insert(node.clone(), elapsed);
    elapsed
}

/// Aggregates stable template rows from rebuildable run/attempt projections.
#[must_use]
pub fn graph_template_rollups(
    runs: &[GraphRunRow],
    attempts: &[GraphNodeAttemptRow],
) -> Vec<GraphTemplateRollup> {
    let mut rollups = BTreeMap::<(String, u32, String), GraphTemplateRollup>::new();
    let run_keys = runs
        .iter()
        .filter(|run| {
            !run.scope
                .as_ref()
                .is_some_and(GraphRunScope::is_run_set_aggregate)
        })
        .map(|run| {
            (
                (run.session_id.clone(), run.graph_id.clone()),
                (
                    run.template.clone(),
                    run.template_version,
                    run.digest.clone(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    for run in runs {
        if run
            .scope
            .as_ref()
            .is_some_and(GraphRunScope::is_run_set_aggregate)
        {
            continue;
        }
        let key = (
            run.template.clone(),
            run.template_version,
            run.digest.clone(),
        );
        let rollup = rollups.entry(key).or_insert_with(|| GraphTemplateRollup {
            template: run.template.clone(),
            template_version: run.template_version,
            digest: run.digest.clone(),
            runs: 0,
            active: 0,
            blocked: 0,
            completed: 0,
            abandoned: 0,
            superseded: 0,
            completion_rate_basis_points: 0,
            abandon_rate_basis_points: 0,
            mis_gate_count: 0,
            override_count: 0,
            node_attempts: 0,
            declared_nodes: 0,
            attempts_per_node_millis: 0,
            node_wall_ms: 0,
            critical_path_elapsed_ms: 0,
            nodes: Vec::new(),
        });
        rollup.runs = rollup.runs.saturating_add(1);
        match run.phase {
            GraphPhase::Active => rollup.active = rollup.active.saturating_add(1),
            GraphPhase::Blocked => rollup.blocked = rollup.blocked.saturating_add(1),
            GraphPhase::Completed => rollup.completed = rollup.completed.saturating_add(1),
            GraphPhase::Abandoned => rollup.abandoned = rollup.abandoned.saturating_add(1),
            GraphPhase::Superseded => rollup.superseded = rollup.superseded.saturating_add(1),
        }
        rollup.mis_gate_count = rollup
            .mis_gate_count
            .saturating_add(u64::from(run.mis_gate_count));
        rollup.override_count = rollup
            .override_count
            .saturating_add(u64::from(run.override_count));
        rollup.declared_nodes = rollup
            .declared_nodes
            .saturating_add(u64::from(run.declared_nodes));
        rollup.critical_path_elapsed_ms = rollup
            .critical_path_elapsed_ms
            .saturating_add(run.critical_path_elapsed_ms);
    }
    let mut nodes = BTreeMap::<(String, u32, String, GraphNodeName), GraphNodeRollup>::new();
    for attempt in attempts {
        let Some((template, version, digest)) =
            run_keys.get(&(attempt.session_id.clone(), attempt.graph_id.clone()))
        else {
            continue;
        };
        let rollup_key = (template.clone(), *version, digest.clone());
        if let Some(rollup) = rollups.get_mut(&rollup_key) {
            rollup.node_attempts = rollup.node_attempts.saturating_add(1);
            rollup.node_wall_ms = rollup.node_wall_ms.saturating_add(attempt.wall_ms);
        }
        let node = nodes
            .entry((
                template.clone(),
                *version,
                digest.clone(),
                attempt.node.clone(),
            ))
            .or_insert_with(|| GraphNodeRollup {
                node: attempt.node.clone(),
                attempts: 0,
                wall_ms: 0,
            });
        node.attempts = node.attempts.saturating_add(1);
        node.wall_ms = node.wall_ms.saturating_add(attempt.wall_ms);
    }
    for ((template, version, digest, _), node) in nodes {
        if let Some(rollup) = rollups.get_mut(&(template, version, digest)) {
            rollup.nodes.push(node);
        }
    }
    for rollup in rollups.values_mut() {
        rollup.completion_rate_basis_points = u32::try_from(
            rollup
                .completed
                .saturating_mul(10_000)
                .checked_div(rollup.runs)
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX);
        rollup.abandon_rate_basis_points = u32::try_from(
            rollup
                .abandoned
                .saturating_mul(10_000)
                .checked_div(rollup.runs)
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX);
        rollup.attempts_per_node_millis = rollup
            .node_attempts
            .saturating_mul(1_000)
            .checked_div(rollup.declared_nodes)
            .unwrap_or(0);
    }
    rollups.into_values().collect()
}

fn graph_reduction_payload(payload: &serde_json::Value) -> Option<EventPayload> {
    let kind = payload.get("type")?.as_str()?;
    if !kind.starts_with("graph_")
        && kind != "todo_graph_attached"
        && kind != "evidence_recorded"
        && !kind.starts_with("menu_")
    {
        return None;
    }
    serde_json::from_value(payload.clone()).ok()
}

#[must_use]
pub fn ship_loop_nodes() -> Vec<GraphNodeSpec> {
    vec![
        GraphNodeSpec {
            name: build_node(),
            gate: GraphGateKind::CommandGreen,
            executor: GraphExecutorShape::Inline,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
            depends_on: Vec::new(),
            red_target: None,
            verify_slots: Vec::new(),
        },
        GraphNodeSpec {
            name: verify_node(),
            gate: GraphGateKind::AllOfN { n: 3 },
            executor: GraphExecutorShape::FanOut,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
            depends_on: vec![build_node()],
            red_target: None,
            verify_slots: ["tests", "lint", "typecheck"]
                .into_iter()
                .map(|id| EvidenceSlotSpec {
                    id: id.into(),
                    authority: EvidenceAuthority::DaemonVerified,
                    subject_selector: SubjectSelector::Command,
                })
                .collect(),
        },
        GraphNodeSpec {
            name: ship_node(),
            gate: GraphGateKind::HumanConfirm,
            executor: GraphExecutorShape::Human,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: None,
            depends_on: vec![verify_node()],
            red_target: None,
            verify_slots: Vec::new(),
        },
    ]
}

#[must_use]
pub fn ship_loop_template() -> GraphTemplateSpec {
    GraphTemplateSpec {
        name: SHIP_LOOP_TEMPLATE.into(),
        version: GRAPH_TEMPLATE_VERSION,
        start_node: Some(build_node()),
        nodes: ship_loop_nodes(),
    }
}

#[must_use]
pub fn graph_template_catalog() -> Vec<GraphTemplateSpec> {
    built_in_workflow_catalog()
        .into_iter()
        .filter(|entry| entry.main_session_eligible)
        .map(|entry| entry.template)
        .collect()
}

/// Complete release-owned workflow catalog. The main-session catalog remains
/// the five historical templates returned by [`graph_template_catalog`]; the
/// adjacent sparse/deeper templates are selectable only for delegated work.
#[must_use]
pub fn built_in_workflow_catalog() -> Vec<BuiltInWorkflowCatalogEntry> {
    let mut catalog = [
        ship_loop_template(),
        super_ship_loop_template(),
        staggered_template(),
        sec_audit_template(),
        docs_sweep_template(),
    ]
    .into_iter()
    .map(|template| BuiltInWorkflowCatalogEntry {
        template,
        main_session_eligible: true,
    })
    .collect::<Vec<_>>();
    catalog.extend(
        [implement_verify_child_template(), deeper_child_template()]
            .into_iter()
            .map(|template| BuiltInWorkflowCatalogEntry {
                template,
                main_session_eligible: false,
            }),
    );
    catalog
}

#[must_use]
pub fn graph_template(name: &str) -> Option<GraphTemplateSpec> {
    match name {
        IMPLEMENT_VERIFY_CHILD_TEMPLATE => Some(implement_verify_child_template()),
        DEEPER_CHILD_TEMPLATE => Some(deeper_child_template()),
        _ => graph_template_catalog()
            .into_iter()
            .find(|template| template.name == name),
    }
}

/// Sparse two-phase child workflow: mutation testimony followed by a fresh
/// daemon-observed verification command.
#[must_use]
pub fn implement_verify_child_template() -> GraphTemplateSpec {
    template(
        IMPLEMENT_VERIFY_CHILD_TEMPLATE,
        "IMPLEMENT",
        vec![
            node_spec(
                "IMPLEMENT",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &[],
                vec![EvidenceSlotSpec {
                    id: "mutation".into(),
                    authority: EvidenceAuthority::ModelAttested,
                    subject_selector: SubjectSelector::WorkspaceRevision,
                }],
            ),
            node_spec(
                "VERIFY",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["IMPLEMENT"],
                vec![daemon_slot("verification")],
            ),
        ],
    )
}

/// Deeper child workflow reserved for genuine dependent phases, fan-out,
/// distinct review, or crash recovery.
#[must_use]
pub fn deeper_child_template() -> GraphTemplateSpec {
    template(
        DEEPER_CHILD_TEMPLATE,
        "PLAN",
        vec![
            node_spec(
                "PLAN",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &[],
                vec![model_slot("plan")],
            ),
            node_spec(
                "IMPLEMENT",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["PLAN"],
                vec![EvidenceSlotSpec {
                    id: "mutation".into(),
                    authority: EvidenceAuthority::ModelAttested,
                    subject_selector: SubjectSelector::WorkspaceRevision,
                }],
            ),
            node_spec(
                "VERIFY",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::FanOut,
                &["IMPLEMENT"],
                vec![daemon_slot("verification")],
            ),
            node_spec(
                "REVIEW",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["VERIFY"],
                vec![model_slot("review")],
            ),
        ],
    )
}

/// Cache structure intentionally omits node names and dependency edges.
#[must_use]
pub fn child_gate_structure(template: &GraphTemplateSpec) -> String {
    template
        .nodes
        .iter()
        .map(|node| {
            let gate = match node.gate {
                GraphGateKind::CommandGreen => "command-green".to_owned(),
                GraphGateKind::AllOfN { n } => format!("all-of-{n}"),
                GraphGateKind::HumanConfirm => "human-confirm".to_owned(),
            };
            let slots = node
                .verify_slots
                .iter()
                .map(|slot| format!("{:?}:{:?}", slot.authority, slot.subject_selector))
                .collect::<Vec<_>>()
                .join(",");
            format!("{gate}[{slots}]")
        })
        .collect::<Vec<_>>()
        .join(">")
}

#[must_use]
pub fn graph_template_digest(template: &GraphTemplateSpec) -> String {
    // v0.0.913 shipped this identity before version/start became explicit.
    // Exact structural equality makes the compatibility value sensitive to
    // every field: any mutation falls through to the whole-template hash.
    if template == &ship_loop_template() {
        return ship_loop_digest();
    }
    let bytes = serde_json::to_vec(template).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

#[must_use]
pub fn ship_loop_digest() -> String {
    // The name and every executable bound are part of immutable template
    // identity. A semantic template edit must mint a different digest.
    let bytes = serde_json::to_vec(&(SHIP_LOOP_TEMPLATE, ship_loop_nodes())).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
}

#[allow(clippy::too_many_lines)]
pub fn validate_graph_template(template: &GraphTemplateSpec) -> Result<(), GraphTemplateError> {
    let reject = |kind, message: String| GraphTemplateError { kind, message };
    if template.name.is_empty()
        || template.name.len() > 64
        || template.version == 0
        || template.nodes.is_empty()
        || template.nodes.len() > GRAPH_MAX_NODES
    {
        return Err(reject(
            GraphTemplateRejection::OverCeiling,
            "graph template name/version/node count is outside its bounded ceiling".into(),
        ));
    }
    let Some(start_node) = template.start_node.as_ref() else {
        return Err(reject(
            GraphTemplateRejection::NoStart,
            "graph template does not declare a start node".into(),
        ));
    };
    let mut names = HashSet::new();
    for node in &template.nodes {
        if !names.insert(node.name.clone()) {
            return Err(reject(
                GraphTemplateRejection::DuplicateNode,
                format!("graph template repeats node {}", node.name),
            ));
        }
    }
    let Some(start_spec) = template.nodes.iter().find(|node| &node.name == start_node) else {
        return Err(reject(
            GraphTemplateRejection::NoStart,
            format!("declared start node {start_node} is absent from the graph template"),
        ));
    };
    if !start_spec.depends_on.is_empty() {
        return Err(reject(
            GraphTemplateRejection::NoStart,
            format!("declared start node {start_node} has incoming dependencies"),
        ));
    }

    let edge_count = template.nodes.iter().try_fold(0_usize, |total, node| {
        total.checked_add(node.depends_on.len())
    });
    let slot_count = template.nodes.iter().try_fold(0_usize, |total, node| {
        total.checked_add(node.verify_slots.len())
    });
    if edge_count.is_none_or(|count| count > GRAPH_MAX_EDGES)
        || slot_count.is_none_or(|count| count > GRAPH_MAX_SLOTS)
    {
        return Err(reject(
            GraphTemplateRejection::OverCeiling,
            "graph template exceeds its edge or evidence-slot ceiling".into(),
        ));
    }

    for node in &template.nodes {
        if node.max_attempts == 0 || node.max_attempts > GRAPH_MAX_ATTEMPTS {
            return Err(reject(
                GraphTemplateRejection::OverCeiling,
                format!("graph node {} exceeds the attempt ceiling", node.name),
            ));
        }
        if node
            .max_evidence_per_attempt
            .is_some_and(|limit| limit == 0 || limit > GRAPH_MAX_EVIDENCE_PER_ATTEMPT)
        {
            return Err(reject(
                GraphTemplateRejection::OverCeiling,
                format!("graph node {} exceeds the evidence ceiling", node.name),
            ));
        }
        let mut dependencies = HashSet::new();
        for dependency in &node.depends_on {
            if !names.contains(dependency) {
                return Err(reject(
                    GraphTemplateRejection::UnknownDependency,
                    format!(
                        "graph node {} depends on unknown node {dependency}",
                        node.name
                    ),
                ));
            }
            if !dependencies.insert(dependency) {
                return Err(reject(
                    GraphTemplateRejection::DuplicateNode,
                    format!("graph node {} repeats dependency {dependency}", node.name),
                ));
            }
        }
        if let Some(target) = &node.red_target {
            if !names.contains(target) {
                return Err(reject(
                    GraphTemplateRejection::UnknownDependency,
                    format!(
                        "graph node {} has an unknown red target {target}",
                        node.name
                    ),
                ));
            }
            if matches!(node.gate, GraphGateKind::HumanConfirm) {
                return Err(reject(
                    GraphTemplateRejection::InvalidGate,
                    format!("human graph node {} cannot declare a red target", node.name),
                ));
            }
            if target != &node.name
                && !graph_dependency_ancestors(&template.nodes, &node.name).contains(target)
            {
                return Err(reject(
                    GraphTemplateRejection::InvalidGate,
                    format!(
                        "graph node {} red target {target} is not a dependency ancestor",
                        node.name
                    ),
                ));
            }
        }
        let mut slots = HashSet::new();
        for slot in &node.verify_slots {
            if slot.id.is_empty()
                || slot.id.len() > 64
                || !slot
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || !slots.insert(slot.id.as_str())
            {
                return Err(reject(
                    GraphTemplateRejection::InvalidGate,
                    format!(
                        "graph node {} has an invalid or duplicate evidence slot",
                        node.name
                    ),
                ));
            }
        }
        match &node.gate {
            GraphGateKind::AllOfN { n } => {
                let slot_len = u32::try_from(node.verify_slots.len()).unwrap_or(u32::MAX);
                if *n == 0 || (!node.verify_slots.is_empty() && *n != slot_len) {
                    return Err(reject(
                        GraphTemplateRejection::InvalidGate,
                        format!("graph node {} has an incoherent all-of-n gate", node.name),
                    ));
                }
            }
            GraphGateKind::HumanConfirm
                if node.executor != GraphExecutorShape::Human
                    || node.max_evidence_per_attempt.is_some()
                    || !node.verify_slots.is_empty() =>
            {
                return Err(reject(
                    GraphTemplateRejection::InvalidGate,
                    format!("graph node {} has an incoherent human gate", node.name),
                ));
            }
            GraphGateKind::CommandGreen | GraphGateKind::HumanConfirm => {}
        }
    }

    let mut indegree = template
        .nodes
        .iter()
        .map(|node| (node.name.clone(), node.depends_on.len()))
        .collect::<HashMap<_, _>>();
    let mut queue = template
        .nodes
        .iter()
        .filter(|node| node.depends_on.is_empty())
        .map(|node| node.name.clone())
        .collect::<VecDeque<_>>();
    let mut visited = 0_usize;
    while let Some(node) = queue.pop_front() {
        visited += 1;
        for dependent in template.nodes.iter().filter(|candidate| {
            candidate
                .depends_on
                .iter()
                .any(|dependency| dependency == &node)
        }) {
            if let Some(count) = indegree.get_mut(&dependent.name) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    queue.push_back(dependent.name.clone());
                }
            }
        }
    }
    if visited != template.nodes.len() {
        return Err(reject(
            GraphTemplateRejection::Cycle,
            "graph template contains a dependency cycle".into(),
        ));
    }

    let mut reachable = HashSet::from([start_node.clone()]);
    let mut queue = VecDeque::from([start_node.clone()]);
    while let Some(node) = queue.pop_front() {
        for dependent in template.nodes.iter().filter(|candidate| {
            candidate
                .depends_on
                .iter()
                .any(|dependency| dependency == &node)
        }) {
            if reachable.insert(dependent.name.clone()) {
                queue.push_back(dependent.name.clone());
            }
        }
    }
    if let Some(node) = template
        .nodes
        .iter()
        .find(|node| !reachable.contains(&node.name))
    {
        return Err(reject(
            GraphTemplateRejection::UnreachableNode,
            format!("graph node {} is unreachable from {start_node}", node.name),
        ));
    }
    Ok(())
}

fn graph_dependency_ancestors(
    specs: &[GraphNodeSpec],
    node: &GraphNodeName,
) -> HashSet<GraphNodeName> {
    let mut ancestors = HashSet::new();
    let mut queue = specs
        .iter()
        .find(|spec| &spec.name == node)
        .map_or_else(VecDeque::new, |spec| {
            VecDeque::from(spec.depends_on.clone())
        });
    while let Some(dependency) = queue.pop_front() {
        if !ancestors.insert(dependency.clone()) {
            continue;
        }
        if let Some(spec) = specs.iter().find(|spec| spec.name == dependency) {
            queue.extend(spec.depends_on.iter().cloned());
        }
    }
    ancestors
}

fn node_spec(
    name: &str,
    gate: GraphGateKind,
    executor: GraphExecutorShape,
    dependencies: &[&str],
    slots: Vec<EvidenceSlotSpec>,
) -> GraphNodeSpec {
    GraphNodeSpec {
        name: GraphNodeName::new(name).unwrap_or_else(|_| build_node()),
        gate: gate.clone(),
        executor,
        max_attempts: GRAPH_MAX_ATTEMPTS,
        max_evidence_per_attempt: (!matches!(gate, GraphGateKind::HumanConfirm))
            .then_some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
        depends_on: dependencies
            .iter()
            .filter_map(|name| GraphNodeName::new(*name).ok())
            .collect(),
        red_target: None,
        verify_slots: slots,
    }
}

fn daemon_slot(id: &str) -> EvidenceSlotSpec {
    EvidenceSlotSpec {
        id: id.into(),
        authority: EvidenceAuthority::DaemonVerified,
        subject_selector: SubjectSelector::Command,
    }
}

fn model_slot(id: &str) -> EvidenceSlotSpec {
    EvidenceSlotSpec {
        id: id.into(),
        authority: EvidenceAuthority::ModelAttested,
        subject_selector: SubjectSelector::Freeform,
    }
}

fn template(name: &str, start: &str, nodes: Vec<GraphNodeSpec>) -> GraphTemplateSpec {
    GraphTemplateSpec {
        name: name.into(),
        version: GRAPH_TEMPLATE_VERSION,
        start_node: GraphNodeName::new(start).ok(),
        nodes,
    }
}

fn super_ship_loop_template() -> GraphTemplateSpec {
    // Owner-specified five-stage loop (2026-08-20): clean code, tests,
    // implement, verify until SHIP, optimize. Gate attempts supply the
    // "until": a red node re-attempts instead of advancing. OPTIMIZE sits
    // before the human SHIP gate so what ships is what was optimized.
    template(
        SUPER_SHIP_LOOP_TEMPLATE,
        "IMPLEMENT",
        vec![
            node_spec(
                "IMPLEMENT",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
            node_spec(
                "TESTS",
                GraphGateKind::AllOfN { n: 2 },
                GraphExecutorShape::FanOut,
                &["IMPLEMENT"],
                vec![daemon_slot("tests"), daemon_slot("lint")],
            ),
            node_spec(
                "CLEAN",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["IMPLEMENT"],
                vec![model_slot("clean-code")],
            ),
            node_spec(
                "OPTIMIZE",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["TESTS", "CLEAN"],
                vec![model_slot("optimize")],
            ),
            node_spec(
                "SHIP",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                &["OPTIMIZE"],
                vec![],
            ),
        ],
    )
}

fn staggered_template() -> GraphTemplateSpec {
    template(
        STAGGERED_TEMPLATE,
        "START",
        vec![
            node_spec(
                "START",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
            node_spec(
                "BACKEND",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["START"],
                vec![],
            ),
            node_spec(
                "FRONTEND",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["START"],
                vec![],
            ),
            node_spec(
                "INTEGRATE",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["BACKEND", "FRONTEND"],
                vec![],
            ),
            node_spec(
                "VERIFY",
                GraphGateKind::AllOfN { n: 2 },
                GraphExecutorShape::FanOut,
                &["INTEGRATE"],
                vec![daemon_slot("tests"), daemon_slot("typecheck")],
            ),
            node_spec(
                "SHIP",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                &["VERIFY"],
                vec![],
            ),
        ],
    )
}

fn sec_audit_template() -> GraphTemplateSpec {
    template(
        SEC_AUDIT_TEMPLATE,
        "SCOPE",
        vec![
            node_spec(
                "SCOPE",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
            node_spec(
                "STATIC",
                GraphGateKind::AllOfN { n: 2 },
                GraphExecutorShape::FanOut,
                &["SCOPE"],
                vec![daemon_slot("scanner"), daemon_slot("dependencies")],
            ),
            node_spec(
                "THREAT_REVIEW",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["SCOPE"],
                vec![model_slot("threat_model")],
            ),
            node_spec(
                "REMEDIATE",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["STATIC", "THREAT_REVIEW"],
                vec![],
            ),
            node_spec(
                "VERIFY",
                GraphGateKind::AllOfN { n: 2 },
                GraphExecutorShape::FanOut,
                &["REMEDIATE"],
                vec![daemon_slot("tests"), daemon_slot("rescan")],
            ),
            node_spec(
                "APPROVE",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                &["VERIFY"],
                vec![],
            ),
        ],
    )
}

fn docs_sweep_template() -> GraphTemplateSpec {
    template(
        DOCS_SWEEP_TEMPLATE,
        "INVENTORY",
        vec![
            node_spec(
                "INVENTORY",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
            node_spec(
                "GUIDES",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["INVENTORY"],
                vec![],
            ),
            node_spec(
                "API_DOCS",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["INVENTORY"],
                vec![],
            ),
            node_spec(
                "LINKS",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["GUIDES", "API_DOCS"],
                vec![daemon_slot("linkcheck")],
            ),
            node_spec(
                "REVIEW",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["GUIDES", "API_DOCS"],
                vec![model_slot("editorial")],
            ),
            node_spec(
                "PUBLISH",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                &["LINKS", "REVIEW"],
                vec![],
            ),
        ],
    )
}

const fn is_zero(value: &u32) -> bool {
    *value == 0
}

const fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Normalizes testimony before both bounded storage and fingerprinting.
#[must_use]
pub fn normalize_evidence_detail(detail: &str) -> String {
    let collapsed = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    bound_utf8(&collapsed, GRAPH_EVIDENCE_DETAIL_MAX_BYTES)
}

#[must_use]
pub fn evidence_fingerprint(normalized_detail: &str) -> String {
    blake3::hash(normalized_detail.as_bytes())
        .to_hex()
        .to_string()
}

/// Derives the bounded subject proxy used until a sealed workspace revision
/// producer exists. Length-prefixing prevents ambiguous concatenations.
#[must_use]
pub fn process_signal_subject_digest(
    command_arg_digest: &str,
    transcript_digest: &str,
    workspace_revision: Option<&WorkspaceRevision>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.process-signal.subject.v1");
    for value in [
        command_arg_digest,
        transcript_digest,
        workspace_revision.map_or("", WorkspaceRevision::as_str),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Deterministic identity for the run-set opened by one exact Plan fact. The
/// event coordinate keeps whole-list replacements distinct even though G1
/// deliberately reuses the Plan `ItemId` inside a lifecycle.
#[must_use]
pub fn todo_run_set_id(
    session_id: &SessionId,
    plan_item_id: &ItemId,
    plan_event_id: &EventId,
) -> GraphRunSetId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.todo-run-set.v1");
    for value in [
        session_id.as_str(),
        plan_item_id.as_str(),
        plan_event_id.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    GraphRunSetId::new(format!("todo-run-set-{}", hasher.finalize().to_hex()))
}

/// Deterministic child graph identity for one frozen todo attachment.
#[must_use]
pub fn todo_child_graph_id(
    session_id: &SessionId,
    run_set_id: &GraphRunSetId,
    plan_item_id: &ItemId,
    todo_id: u32,
) -> GraphId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"haider.todo-child-graph.v1");
    for value in [
        session_id.as_str(),
        run_set_id.as_str(),
        plan_item_id.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(&todo_id.to_be_bytes());
    GraphId::new(format!("todo-graph-{}", hasher.finalize().to_hex()))
}

#[must_use]
fn bound_utf8(value: &str, max_bytes: usize) -> String {
    let mut bounded = value.to_owned();
    truncate_utf8(&mut bounded, max_bytes);
    bounded
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn graph_fact(seq: u64, payload: EventPayload) -> RawEnvelope {
        crate::envelope::EventEnvelope {
            schema_version: crate::envelope::SCHEMA_VERSION,
            event_id: EventId::new(format!("m2d-fact-{seq}")),
            seq,
            session_id: SessionId::new("m2d-session"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: crate::ids::DeviceId::new("m2d-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: seq.saturating_mul(10),
            render: crate::envelope::RenderTargets {
                ui: true,
                durable: true,
                prompt: crate::envelope::PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("graph payload"),
        }
    }

    fn one_node_pin(graph_id: &GraphId) -> GraphPinned {
        GraphPinned {
            graph_id: graph_id.clone(),
            template: "m2d-one-node".into(),
            digest: "m2d-one-node-digest".into(),
            template_version: 1,
            start_node: Some(build_node()),
            nodes: vec![GraphNodeSpec {
                name: build_node(),
                gate: GraphGateKind::CommandGreen,
                executor: GraphExecutorShape::Inline,
                max_attempts: 2,
                max_evidence_per_attempt: Some(2),
                depends_on: Vec::new(),
                red_target: None,
                verify_slots: Vec::new(),
            }],
        }
    }

    fn run_set_prefix(required: u32) -> (Vec<RawEnvelope>, GraphId, GraphRunSetId, ItemId) {
        let root = GraphId::new("m2d-root");
        let run_set = GraphRunSetId::new("m2d-run-set");
        let plan = ItemId::new("m2d-plan");
        let facts = vec![
            graph_fact(1, EventPayload::GraphPinned(one_node_pin(&root))),
            graph_fact(
                2,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: root.clone(),
                    node: build_node(),
                    attempt: 1,
                }),
            ),
            graph_fact(
                3,
                EventPayload::GraphRunSetOpened(GraphRunSetOpened {
                    run_set_id: run_set.clone(),
                    root_graph_id: root.clone(),
                    plan_item_id: plan.clone(),
                    plan_event_id: EventId::new("plan-event-1"),
                    required_children: required,
                }),
            ),
        ];
        (facts, root, run_set, plan)
    }

    fn attach_and_pin(
        facts: &mut Vec<RawEnvelope>,
        run_set: &GraphRunSetId,
        plan: &ItemId,
        todo_id: u32,
        dependency: Option<u32>,
        graph_id: &GraphId,
    ) {
        let seq = u64::try_from(facts.len()).unwrap_or(u64::MAX) + 1;
        facts.push(graph_fact(
            seq,
            EventPayload::TodoGraphAttached(TodoGraphAttached {
                run_set_id: run_set.clone(),
                plan_item_id: plan.clone(),
                todo_id,
                depends_on_todo_id: dependency,
                child_graph_id: graph_id.clone(),
                ordinal: todo_id,
            }),
        ));
        facts.push(graph_fact(
            seq + 1,
            EventPayload::GraphPinned(one_node_pin(graph_id)),
        ));
    }

    fn model_green(graph_id: &GraphId, attempt: u32, detail: &str) -> EvidenceRecorded {
        EvidenceRecorded {
            graph_id: graph_id.clone(),
            node: build_node(),
            attempt,
            verdict: EvidenceVerdict::Green,
            detail: detail.into(),
            fingerprint: evidence_fingerprint(detail),
            slot: None,
            subject_digest: None,
            source: GraphEvidenceSource::Model {
                run_id: RunId::new(format!("run-{detail}")),
                call_id: format!("call-{detail}"),
            },
        }
    }

    #[test]
    fn template_digest_is_stable_and_brief_is_bounded() {
        assert_eq!(ship_loop_digest().len(), 64);
        let status = GraphStatus {
            graph_id: GraphId::new("g"),
            template: SHIP_LOOP_TEMPLATE.into(),
            digest: ship_loop_digest(),
            template_version: GRAPH_TEMPLATE_VERSION,
            start_node: Some(build_node()),
            phase: GraphPhase::Active,
            current_node: Some(verify_node()),
            ready_nodes: vec![verify_node()],
            attempt: 2,
            nodes: ship_loop_nodes()
                .into_iter()
                .map(|node| GraphNodeStatus {
                    node: node.name,
                    gate: Some(node.gate),
                    executor: Some(node.executor),
                    attempts_opened: 1,
                    current_attempt: Some(2),
                    evidence: GraphEvidenceTally::default(),
                    evidence_slots: node
                        .verify_slots
                        .iter()
                        .map(|slot| GraphEvidenceSlotStatus {
                            id: slot.id.clone(),
                            authority: slot.authority,
                            subject_selector: slot.subject_selector,
                            verdict: None,
                            fingerprint: None,
                            subject_digest: None,
                            source: None,
                        })
                        .collect(),
                    satisfied: false,
                })
                .collect(),
            blocked_reason: None,
            pending_menu: None,
            pending_menus: Vec::new(),
            run_set: None,
        };
        let brief = status.graph_brief().unwrap_or_default();
        assert!(!brief.is_empty());
        assert!(brief.contains("VERIFY attempt 2/8"));
        assert!(brief.len() <= GRAPH_BRIEF_MAX_BYTES);
    }

    #[test]
    fn evidence_normalization_is_bounded_and_fingerprint_stable() {
        let a = normalize_evidence_detail("  cargo\r\n test   passed ");
        let b = normalize_evidence_detail("cargo test passed");
        assert_eq!(a, b);
        assert_eq!(evidence_fingerprint(&a), evidence_fingerprint(&b));
        assert!(normalize_evidence_detail(&"🦀".repeat(1_000)).len() <= 1_024);
    }

    #[test]
    fn oversized_graph_brief_keeps_both_ends_with_a_machine_marker() {
        let source = format!(
            "GraphBrief: BUILD {} next: verify the final diagnostic.",
            "middle ".repeat(200)
        );
        let first = bound_graph_brief(source.clone());
        let second = bound_graph_brief(source);
        assert_eq!(first, second);
        assert!(first.len() <= GRAPH_BRIEF_MAX_BYTES);
        assert!(first.starts_with("GraphBrief: BUILD"));
        assert!(first.ends_with("next: verify the final diagnostic."));
        assert!(first.contains("\"haider_elision_v1\""));
        assert!(first.contains("\"scope\":\"graph_brief\""));
    }

    #[test]
    fn m2d_plan_reorder_cannot_retarget_an_existing_child() {
        // Expected failure under mutation: key attachments by array position,
        // or overwrite an earlier `(run-set, Plan ItemId, todo id)` binding.
        let (mut facts, root, run_set_one, plan) = run_set_prefix(1);
        let child_one = GraphId::new("child-one");
        attach_and_pin(&mut facts, &run_set_one, &plan, 7, None, &child_one);
        facts.push(graph_fact(
            6,
            EventPayload::TodoGraphAttached(TodoGraphAttached {
                run_set_id: run_set_one.clone(),
                plan_item_id: plan.clone(),
                todo_id: 7,
                depends_on_todo_id: None,
                child_graph_id: GraphId::new("retargeted-by-reorder"),
                ordinal: 0,
            }),
        ));
        let run_set_two = GraphRunSetId::new("m2d-run-set-two");
        facts.push(graph_fact(
            7,
            EventPayload::GraphRunSetOpened(GraphRunSetOpened {
                run_set_id: run_set_two.clone(),
                root_graph_id: root,
                plan_item_id: plan.clone(),
                plan_event_id: EventId::new("plan-event-2"),
                required_children: 1,
            }),
        ));
        let child_two = GraphId::new("child-two");
        attach_and_pin(&mut facts, &run_set_two, &plan, 7, None, &child_two);

        let reduced = reduce_graphs(&facts);
        assert_eq!(
            reduced.run_sets[&run_set_one].children[0].graph_id,
            child_one
        );
        assert_eq!(
            reduced.run_sets[&run_set_two].children[0].graph_id,
            child_two
        );
        assert_eq!(reduced.active_run_set.as_ref(), Some(&run_set_two));
    }

    #[test]
    fn incremental_reduction_keeps_run_set_and_root_projection_coherent() {
        // The observer retains this projection across calls. Every public
        // incremental fold must therefore equal a from-scratch reduction of
        // the same prefix, including the derived child and aggregate state.
        let (mut facts, root, run_set, plan) = run_set_prefix(1);
        let child = GraphId::new("incremental-child");
        attach_and_pin(&mut facts, &run_set, &plan, 1, None, &child);
        facts.push(graph_fact(
            6,
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: child.clone(),
                node: build_node(),
                attempt: 1,
            }),
        ));
        facts.push(graph_fact(
            7,
            EventPayload::GraphCompleted(GraphCompleted { graph_id: child }),
        ));

        let mut incremental = GraphReductions::default();
        for (index, fact) in facts.iter().enumerate() {
            incremental.apply_envelope(fact);
            assert_eq!(incremental, reduce_graphs(&facts[..=index]));
        }
        assert_eq!(incremental.run_sets[&run_set].terminal_children, 1);
        let root = incremental
            .graph(&root)
            .and_then(|reduction| reduction.status.as_ref())
            .expect("aggregate root");
        assert_eq!(root.phase, GraphPhase::Completed);
        assert_eq!(root.run_set.as_ref(), incremental.run_sets.get(&run_set));
    }

    #[test]
    fn legacy_start_retry_still_invalidates_nodes_without_stamped_dependencies() {
        // Pre-M2b pins did not carry dependency edges. START must therefore
        // retain its historical whole-graph invalidation semantics instead
        // of deriving an incomplete forward slice from empty legacy fields.
        let graph_id = GraphId::new("legacy-start-retry");
        let build = build_node();
        let verify = verify_node();
        let nodes = vec![
            node_spec(
                "BUILD",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
            node_spec(
                "VERIFY",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &[],
                vec![],
            ),
        ];
        let mut verify_green = model_green(&graph_id, 1, "legacy verify");
        verify_green.node = verify.clone();
        let facts = vec![
            graph_fact(
                1,
                EventPayload::GraphPinned(GraphPinned {
                    graph_id: graph_id.clone(),
                    template: "legacy-start-retry".into(),
                    digest: "legacy-start-retry-digest".into(),
                    template_version: 0,
                    start_node: None,
                    nodes,
                }),
            ),
            graph_fact(
                2,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: graph_id.clone(),
                    node: build.clone(),
                    attempt: 1,
                }),
            ),
            graph_fact(
                3,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: graph_id.clone(),
                    node: verify.clone(),
                    attempt: 1,
                }),
            ),
            graph_fact(4, EventPayload::EvidenceRecorded(verify_green)),
            graph_fact(
                5,
                EventPayload::GraphGateSatisfied(GraphGateSatisfied {
                    graph_id: graph_id.clone(),
                    node: verify.clone(),
                    attempt: 1,
                }),
            ),
            graph_fact(
                6,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: graph_id.clone(),
                    node: build.clone(),
                    attempt: 2,
                }),
            ),
        ];

        let reduced = reduce_graphs(&facts);
        let status = reduced
            .graph(&graph_id)
            .and_then(|reduction| reduction.status.as_ref())
            .expect("legacy status");
        assert_eq!(status.current_node, Some(build));
        let verify = status
            .nodes
            .iter()
            .find(|node| node.node == verify)
            .expect("verify state");
        assert_eq!(verify.current_attempt, None);
        assert!(!verify.satisfied);
        assert_eq!(verify.evidence.effective_green, 0);
    }

    #[test]
    fn m2d_retrying_one_child_preserves_its_siblings_green_state() {
        // Expected failure under mutation: share one epoch/frontier across all
        // todos, causing child A's retry to clear child B's satisfied green.
        let (mut facts, _, run_set, plan) = run_set_prefix(2);
        let child_a = GraphId::new("child-a");
        let child_b = GraphId::new("child-b");
        attach_and_pin(&mut facts, &run_set, &plan, 1, None, &child_a);
        attach_and_pin(&mut facts, &run_set, &plan, 2, None, &child_b);
        for child in [&child_a, &child_b] {
            let seq = u64::try_from(facts.len()).unwrap_or(u64::MAX) + 1;
            facts.push(graph_fact(
                seq,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: child.clone(),
                    node: build_node(),
                    attempt: 1,
                }),
            ));
            facts.push(graph_fact(
                seq + 1,
                EventPayload::EvidenceRecorded(model_green(child, 1, child.as_str())),
            ));
            facts.push(graph_fact(
                seq + 2,
                EventPayload::GraphGateSatisfied(GraphGateSatisfied {
                    graph_id: child.clone(),
                    node: build_node(),
                    attempt: 1,
                }),
            ));
        }
        facts.push(graph_fact(
            14,
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: child_a.clone(),
                node: build_node(),
                attempt: 2,
            }),
        ));

        let reduced = reduce_graphs(&facts);
        let sibling = reduced.graph(&child_b).unwrap().status.as_ref().unwrap();
        assert_eq!(sibling.attempt, 1);
        assert_eq!(sibling.nodes[0].current_attempt, Some(1));
        assert_eq!(sibling.current_node, None);
        assert!(sibling.nodes[0].satisfied);
        assert_eq!(sibling.nodes[0].evidence.effective_green, 1);
        let retried = reduced.graph(&child_a).unwrap().status.as_ref().unwrap();
        assert_eq!(retried.attempt, 2);
        assert_eq!(retried.nodes[0].current_attempt, Some(2));
        assert_eq!(retried.current_node, Some(build_node()));
        assert!(!retried.nodes[0].satisfied);
        assert_eq!(retried.nodes[0].evidence.effective_green, 0);
    }

    #[test]
    fn m2d_aggregate_requires_all_k_terminal_child_contracts() {
        // Expected failure under mutation: treat any terminal child, or K-1
        // child contracts, as sufficient to complete the aggregate root.
        let (mut facts, root, run_set, plan) = run_set_prefix(2);
        let child_a = GraphId::new("contract-a");
        let child_b = GraphId::new("contract-b");
        attach_and_pin(&mut facts, &run_set, &plan, 1, None, &child_a);
        attach_and_pin(&mut facts, &run_set, &plan, 2, None, &child_b);
        facts.push(graph_fact(
            8,
            EventPayload::GraphCompleted(GraphCompleted {
                graph_id: child_a.clone(),
            }),
        ));
        let k_minus_one = reduce_graphs(&facts);
        assert_eq!(
            k_minus_one
                .graph(&root)
                .unwrap()
                .status
                .as_ref()
                .unwrap()
                .phase,
            GraphPhase::Active
        );
        assert_eq!(k_minus_one.run_sets[&run_set].terminal_children, 1);

        facts.push(graph_fact(
            9,
            EventPayload::GraphAbandoned(GraphAbandoned {
                graph_id: child_a,
                why: "duplicate terminal contract".into(),
            }),
        ));
        let duplicate_same_child = reduce_graphs(&facts);
        assert_eq!(duplicate_same_child.run_sets[&run_set].terminal_children, 1);
        assert_eq!(
            duplicate_same_child
                .graph(&root)
                .unwrap()
                .status
                .as_ref()
                .unwrap()
                .phase,
            GraphPhase::Active
        );

        facts.push(graph_fact(
            10,
            EventPayload::GraphSuperseded(GraphSuperseded {
                old: child_b,
                new: GraphId::new("contract-b-successor"),
            }),
        ));
        let all_k = reduce_graphs(&facts);
        assert_eq!(
            all_k.graph(&root).unwrap().status.as_ref().unwrap().phase,
            GraphPhase::Completed
        );
        assert_eq!(all_k.run_sets[&run_set].terminal_children, 2);
    }

    #[test]
    fn m2d_child_graphs_apply_epoch_and_dag_laws_independently() {
        // Expected failure under mutation: bypass M2b validation for a child,
        // or retain child A's epoch-1 green after opening its epoch 2.
        let malformed = GraphTemplateSpec {
            name: "bad-child".into(),
            version: 1,
            start_node: Some(GraphNodeName::new("A").unwrap()),
            nodes: vec![GraphNodeSpec {
                name: GraphNodeName::new("A").unwrap(),
                gate: GraphGateKind::CommandGreen,
                executor: GraphExecutorShape::Inline,
                max_attempts: 2,
                max_evidence_per_attempt: Some(1),
                depends_on: vec![GraphNodeName::new("A").unwrap()],
                red_target: None,
                verify_slots: Vec::new(),
            }],
        };
        assert_eq!(
            validate_graph_template(&malformed).unwrap_err().kind,
            GraphTemplateRejection::NoStart
        );

        let (mut facts, _, run_set, plan) = run_set_prefix(2);
        let child_a = GraphId::new("law-child-a");
        let child_b = GraphId::new("law-child-b");
        attach_and_pin(&mut facts, &run_set, &plan, 1, None, &child_a);
        attach_and_pin(&mut facts, &run_set, &plan, 2, None, &child_b);
        for child in [&child_a, &child_b] {
            let seq = u64::try_from(facts.len()).unwrap_or(u64::MAX) + 1;
            facts.push(graph_fact(
                seq,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id: child.clone(),
                    node: build_node(),
                    attempt: 1,
                }),
            ));
            facts.push(graph_fact(
                seq + 1,
                EventPayload::EvidenceRecorded(model_green(child, 1, child.as_str())),
            ));
        }
        facts.push(graph_fact(
            12,
            EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                graph_id: child_a.clone(),
                node: build_node(),
                attempt: 2,
            }),
        ));
        let reduced = reduce_graphs(&facts);
        assert_eq!(
            reduced
                .graph(&child_a)
                .unwrap()
                .status
                .as_ref()
                .unwrap()
                .nodes[0]
                .evidence
                .effective_green,
            0
        );
        assert_eq!(
            reduced
                .graph(&child_b)
                .unwrap()
                .status
                .as_ref()
                .unwrap()
                .nodes[0]
                .evidence
                .effective_green,
            1
        );
    }

    #[test]
    fn m2d_legacy_single_graph_status_bytes_are_unchanged() {
        // Expected failure under mutation: serialize any M2d default field into
        // a journal reduction that contains no run-set facts.
        let graph_id = GraphId::new("legacy");
        let mut legacy_pin = one_node_pin(&graph_id);
        legacy_pin.template = SHIP_LOOP_TEMPLATE.into();
        legacy_pin.digest = "d".into();
        legacy_pin.template_version = 0;
        legacy_pin.start_node = None;
        let facts = vec![
            graph_fact(1, EventPayload::GraphPinned(legacy_pin)),
            graph_fact(
                2,
                EventPayload::GraphAttemptOpened(GraphAttemptOpened {
                    graph_id,
                    node: build_node(),
                    attempt: 1,
                }),
            ),
        ];
        let encoded =
            serde_json::to_string(reduce_graph(&facts).status.as_ref().expect("legacy status"))
                .unwrap();
        assert_eq!(
            encoded,
            r#"{"graph_id":"legacy","template":"ship-loop","digest":"d","phase":"active","current_node":"BUILD","attempt":1,"nodes":[{"node":"BUILD","attempts_opened":1,"current_attempt":1,"evidence":{"green":0,"red":0,"effective_green":0,"standing_red":0},"satisfied":false}]}"#
        );
    }

    #[test]
    fn m2b_node_wire_and_ship_loop_digest_are_legacy_stable() {
        // Mutation guard: changing the newtype serializer or the exact
        // canonical compatibility check must fail the v0.0.913 wire law.
        for legacy in ["BUILD", "VERIFY", "SHIP"] {
            let node: GraphNodeName =
                serde_json::from_str(&format!("\"{legacy}\"")).expect("legacy node decodes");
            assert_eq!(node.as_str(), legacy);
            assert_eq!(
                serde_json::to_string(&node).expect("node encodes"),
                format!("\"{legacy}\"")
            );
        }
        assert_eq!(
            ship_loop_digest(),
            "1c30a48bdb255309f5b69ce5497d99a41b7182b263e55f8bfb5864fdaf4147b9"
        );
        assert_eq!(
            graph_template_digest(&ship_loop_template()),
            ship_loop_digest()
        );
        let mut mutated = ship_loop_template();
        mutated.version += 1;
        assert_ne!(graph_template_digest(&mutated), ship_loop_digest());
    }

    #[test]
    fn m2b_malformed_dags_have_distinct_typed_rejections() {
        let assert_rejection = |template: GraphTemplateSpec, expected| {
            assert_eq!(
                validate_graph_template(&template)
                    .expect_err("malformed template must reject")
                    .kind,
                expected
            );
        };

        // Mutation guard: removing uniqueness validation would admit two
        // durable obligations with the same event coordinates.
        let mut duplicate = ship_loop_template();
        duplicate.nodes.push(duplicate.nodes[0].clone());
        assert_rejection(duplicate, GraphTemplateRejection::DuplicateNode);

        // Mutation guard: inferring START from declaration order would make
        // immutable retry semantics depend on current binary defaults.
        let mut no_start = ship_loop_template();
        no_start.start_node = None;
        assert_rejection(no_start, GraphTemplateRejection::NoStart);

        // Mutation guard: omitting the topological pass admits a dependency
        // knot which can never become ready.
        let cycle = template(
            "cycle",
            "A",
            vec![
                node_spec(
                    "A",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &[],
                    vec![],
                ),
                node_spec(
                    "B",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &["A", "C"],
                    vec![],
                ),
                node_spec(
                    "C",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &["B"],
                    vec![],
                ),
            ],
        );
        assert_rejection(cycle, GraphTemplateRejection::Cycle);

        // Mutation guard: accepting a second root creates an obligation that
        // is unreachable from the declared retry START.
        let unreachable = template(
            "unreachable",
            "A",
            vec![
                node_spec(
                    "A",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &[],
                    vec![],
                ),
                node_spec(
                    "B",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &["A"],
                    vec![],
                ),
                node_spec(
                    "ORPHAN",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &[],
                    vec![],
                ),
            ],
        );
        assert_rejection(unreachable, GraphTemplateRejection::UnreachableNode);

        // Mutation guard: silently dropping an unknown edge would change the
        // pinned obligation schema during reduction.
        let unknown = template(
            "unknown",
            "A",
            vec![
                node_spec(
                    "A",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &[],
                    vec![],
                ),
                node_spec(
                    "B",
                    GraphGateKind::CommandGreen,
                    GraphExecutorShape::Inline,
                    &["MISSING"],
                    vec![],
                ),
            ],
        );
        assert_rejection(unknown, GraphTemplateRejection::UnknownDependency);

        // A conditional back-edge may only reopen the same node or one of
        // its dependency ancestors; otherwise it could jump across forks or
        // forward through the DAG without a well-defined invalidation slice.
        let mut non_ancestor_back_edge = ship_loop_template();
        non_ancestor_back_edge.nodes[0].red_target = Some(verify_node());
        assert_rejection(non_ancestor_back_edge, GraphTemplateRejection::InvalidGate);

        // Humans decide explicitly through a menu and never autonomously
        // consume a red condition, so a retry target on a human gate is
        // structurally invalid even when it points to an ancestor.
        let mut human_back_edge = ship_loop_template();
        human_back_edge.nodes[2].red_target = Some(build_node());
        assert_rejection(human_back_edge, GraphTemplateRejection::InvalidGate);

        // Mutation guard: raising or forgetting the 512-node bound permits
        // unbounded reducer state from one pin.
        let over = template(
            "over",
            "N0",
            (0..=GRAPH_MAX_NODES)
                .map(|index| {
                    let name = format!("N{index}");
                    let dependencies = (index > 0)
                        .then(|| format!("N{}", index - 1))
                        .into_iter()
                        .collect::<Vec<_>>();
                    GraphNodeSpec {
                        name: GraphNodeName::new(name).expect("bounded generated node"),
                        gate: GraphGateKind::CommandGreen,
                        executor: GraphExecutorShape::Inline,
                        max_attempts: GRAPH_MAX_ATTEMPTS,
                        max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
                        depends_on: dependencies
                            .iter()
                            .map(|dependency| {
                                GraphNodeName::new(dependency.clone()).expect("bounded dependency")
                            })
                            .collect(),
                        red_target: None,
                        verify_slots: Vec::new(),
                    }
                })
                .collect(),
        );
        assert_rejection(over, GraphTemplateRejection::OverCeiling);
    }

    #[test]
    fn m2b_catalog_is_five_valid_immutable_templates() {
        // Mutation guard: changing catalog data without maintaining DAG laws
        // or whole-template identity must fail this catalog-wide check.
        let catalog = graph_template_catalog();
        assert_eq!(catalog.len(), 5);
        let mut digests = HashSet::new();
        for template in catalog {
            validate_graph_template(&template).expect("catalog template validates");
            assert!(digests.insert(graph_template_digest(&template)));
        }
        let complete = built_in_workflow_catalog();
        assert_eq!(complete.len(), 7);
        assert!(
            complete[..5]
                .iter()
                .all(|entry| entry.main_session_eligible)
        );
        assert!(
            complete[5..]
                .iter()
                .all(|entry| !entry.main_session_eligible)
        );
        assert_eq!(complete[5].template.name, IMPLEMENT_VERIFY_CHILD_TEMPLATE);
        assert_eq!(complete[6].template.name, DEEPER_CHILD_TEMPLATE);
    }

    #[test]
    fn m2e_decision_gate_defaults_bare_and_requires_exact_triggers() {
        // MUTATION CHECK: make any omitted selector grant a graph. Expected
        // failure: the first decision below is no longer one bare attempt.
        let default = decide_child_workflow(None, None, true);
        assert!(default.is_bare());
        assert_eq!(default.reason, "default_bare_attempt");
        assert!(!default.workflow_author);

        // MUTATION CHECK: accept implement_verify without mutation plus
        // independent verification. Expected failure: wrong trigger grants.
        let implement = ChildWorkflowSelector::ImplementVerify;
        assert!(
            decide_child_workflow(Some(&implement), Some(ChildWorkflowTrigger::FanOut), false,)
                .is_bare()
        );
        assert!(
            !decide_child_workflow(
                Some(&implement),
                Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
                false,
            )
            .is_bare()
        );

        // MUTATION CHECK: grant deeper/authoring on the mutation-only trigger.
        // Expected failure: deeper or workflow_author becomes active below.
        let deeper = ChildWorkflowSelector::Deeper;
        assert!(
            decide_child_workflow(
                Some(&deeper),
                Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
                true,
            )
            .is_bare()
        );
        let fan_out =
            decide_child_workflow(Some(&deeper), Some(ChildWorkflowTrigger::FanOut), true);
        assert!(!fan_out.is_bare());
        assert!(fan_out.workflow_author);

        // MUTATION CHECK: let workflow_ref bypass the referenced template's
        // class. Expected failure: either mismatched reference gains a graph.
        let deeper_ref = ChildWorkflowSelector::WorkflowRef(DEEPER_CHILD_TEMPLATE.into());
        assert!(
            decide_child_workflow(
                Some(&deeper_ref),
                Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
                false,
            )
            .is_bare()
        );
        let implement_ref =
            ChildWorkflowSelector::WorkflowRef(IMPLEMENT_VERIFY_CHILD_TEMPLATE.into());
        assert!(
            decide_child_workflow(
                Some(&implement_ref),
                Some(ChildWorkflowTrigger::DistinctReview),
                false,
            )
            .is_bare()
        );
        assert!(
            !decide_child_workflow(
                Some(&implement_ref),
                Some(ChildWorkflowTrigger::MutationWithIndependentVerification),
                false,
            )
            .is_bare()
        );
    }

    #[test]
    fn m2e_child_templates_are_bounded_non_human_graphs() {
        for template in [implement_verify_child_template(), deeper_child_template()] {
            validate_graph_template(&template).expect("child template validates");
            assert!(
                template
                    .nodes
                    .iter()
                    .all(|node| !matches!(node.gate, GraphGateKind::HumanConfirm))
            );
        }
    }

    #[test]
    fn m2e_child_attachment_is_not_a_cross_graph_edge() {
        // MUTATION CHECK: teach reduce_graphs to adopt ChildGraphAttached as a
        // child DAG edge. Expected failure: the child appears in by_graph.
        let parent = GraphId::new("parent-graph");
        let child = GraphId::new("child-graph");
        let template = implement_verify_child_template();
        let key = ChildTemplateCacheKey {
            task_shape: "mutation_verify".into(),
            effective_grant_digest: "grant".into(),
            gate_structure: child_gate_structure(&template),
        };
        let facts = vec![
            graph_fact(
                1,
                EventPayload::GraphPinned(GraphPinned {
                    graph_id: parent.clone(),
                    template: template.name.clone(),
                    digest: graph_template_digest(&template),
                    template_version: template.version,
                    start_node: template.start_node.clone(),
                    nodes: template.nodes.clone(),
                }),
            ),
            graph_fact(
                2,
                EventPayload::ChildGraphAttached(ChildGraphAttached {
                    parent_run_id: RunId::new("parent-run"),
                    parent_call_id: "call".into(),
                    parent_tool_item_id: ItemId::new("tool"),
                    parent_attempt: ParentGraphAttempt {
                        graph_id: parent.clone(),
                        node: GraphNodeName::new("IMPLEMENT").expect("node"),
                        attempt: 1,
                    },
                    parent_slot: "mutation".into(),
                    parent_authority: EvidenceAuthority::ModelAttested,
                    child_session_id: SessionId::new("child-session"),
                    child_run_id: RunId::new("child-run"),
                    child_graph_id: child.clone(),
                    workflow: ChildWorkflowSelector::ImplementVerify,
                    template: template.name.clone(),
                    digest: graph_template_digest(&template),
                    gate_reason: "mutation_with_independent_verification".into(),
                    cache_key: key,
                    cache_hit: false,
                    workflow_author: false,
                }),
            ),
        ];
        let reduced = reduce_graphs(&facts);
        assert!(reduced.graph(&parent).is_some());
        assert!(reduced.graph(&child).is_none());
    }

    /// Loom child-workflow gap (935): a registered loom workflow id resolves
    /// as a child template ONLY when a trigger is present AND the registry
    /// confirms it (`registered_workflow_ref`); otherwise it stays an honest
    /// bare attempt with a named reason — never a silent unauthorized run.
    ///
    /// MUTATION CHECK: resolve a WorkflowRef without the registry gate (drop
    /// the `registered_workflow_ref` guard). Expected failure: an
    /// unregistered ref resolves to a template instead of bare-attempt.
    #[test]
    fn registered_loom_workflow_ref_resolves_only_with_registry_and_trigger() {
        let sel = ChildWorkflowSelector::WorkflowRef("my-loom-flow".into());
        // Registered + trigger present → resolves to the named template.
        let d = decide_child_workflow_with_registry(
            Some(&sel),
            Some(ChildWorkflowTrigger::DependentPhases),
            false,
            true,
        );
        assert_eq!(d.template.as_deref(), Some("my-loom-flow"));
        assert_eq!(d.reason, "registered_loom_workflow_ref");
        // NOT registered → honest bare attempt, never a silent run.
        let d = decide_child_workflow_with_registry(
            Some(&sel),
            Some(ChildWorkflowTrigger::DependentPhases),
            false,
            false,
        );
        assert_eq!(d.template, None);
        assert_eq!(d.reason, "workflow_ref_not_registered_child_template");
        // No trigger → refused regardless of registration.
        let d = decide_child_workflow_with_registry(Some(&sel), None, false, true);
        assert_eq!(d.template, None);
        assert_eq!(d.reason, "missing_workflow_trigger");
    }

    fn activation_node(value: &str) -> GraphNodeName {
        GraphNodeName::new(value).expect("activation node")
    }

    fn activation_evidence(
        marker: char,
        evidence_type: &str,
        parents: Vec<ArtifactRef>,
    ) -> InstructEvidenceRef {
        InstructEvidenceRef::new(
            ArtifactRef::new(format!("blake3:{}", marker.to_string().repeat(64))),
            evidence_type,
            1,
            parents,
        )
    }

    fn diamond_activation_ast() -> WorkflowActivationAst {
        let root = activation_node("ROOT");
        let left = activation_node("LEFT");
        let right = activation_node("RIGHT");
        let join = activation_node("JOIN");
        WorkflowActivationAst {
            workflow_id: "activation-diamond".into(),
            workflow_digest: "blake3:compiled-diamond".into(),
            input_type: "Question".into(),
            output_type: "Answer".into(),
            nodes: vec![
                WorkflowActivationNode {
                    node: root.clone(),
                    input_type: "Question".into(),
                    output_type: "RootFact".into(),
                    join: WorkflowJoinSemantics {
                        initial_all: vec![1],
                        reactivate_any: Vec::new(),
                    },
                    convergence_gate: false,
                },
                WorkflowActivationNode {
                    node: left.clone(),
                    input_type: "RootFact".into(),
                    output_type: "LeftFact".into(),
                    join: WorkflowJoinSemantics {
                        initial_all: vec![2],
                        reactivate_any: Vec::new(),
                    },
                    convergence_gate: false,
                },
                WorkflowActivationNode {
                    node: right.clone(),
                    input_type: "RootFact".into(),
                    output_type: "RightFact".into(),
                    join: WorkflowJoinSemantics {
                        initial_all: vec![3],
                        reactivate_any: Vec::new(),
                    },
                    convergence_gate: false,
                },
                WorkflowActivationNode {
                    node: join.clone(),
                    input_type: "LeftFact + RightFact".into(),
                    output_type: "Answer".into(),
                    join: WorkflowJoinSemantics {
                        initial_all: vec![4, 5],
                        reactivate_any: Vec::new(),
                    },
                    convergence_gate: false,
                },
            ],
            edges: vec![
                WorkflowActivationEdge {
                    id: 1,
                    kind: WorkflowEdgeKind::GraphInput,
                    from: None,
                    to: root.clone(),
                    evidence_type: "Question".into(),
                },
                WorkflowActivationEdge {
                    id: 2,
                    kind: WorkflowEdgeKind::Forward,
                    from: Some(root.clone()),
                    to: left,
                    evidence_type: "RootFact".into(),
                },
                WorkflowActivationEdge {
                    id: 3,
                    kind: WorkflowEdgeKind::Forward,
                    from: Some(root),
                    to: right,
                    evidence_type: "RootFact".into(),
                },
                WorkflowActivationEdge {
                    id: 4,
                    kind: WorkflowEdgeKind::Forward,
                    from: Some(activation_node("LEFT")),
                    to: join.clone(),
                    evidence_type: "LeftFact".into(),
                },
                WorkflowActivationEdge {
                    id: 5,
                    kind: WorkflowEdgeKind::Forward,
                    from: Some(activation_node("RIGHT")),
                    to: join,
                    evidence_type: "RightFact".into(),
                },
            ],
            max_back_edge_activations: 1,
        }
    }

    fn started_activation_state(ast: WorkflowActivationAst) -> WorkflowGraphState {
        let seed = activation_evidence('a', &ast.input_type, Vec::new());
        WorkflowGraphState::from_started(
            1,
            WorkflowGraphStarted {
                graph_id: GraphId::new("activation-graph"),
                ast_digest: workflow_activation_ast_digest(&ast),
                ast,
                seed: Some(seed),
            },
        )
        .expect("valid activation graph")
    }

    fn activation_seed(state: &WorkflowGraphState) -> InstructEvidenceRef {
        state.seed.clone().expect("activation seed")
    }

    fn activation_event(
        state: &WorkflowGraphState,
        node: &str,
        cause: WorkflowActivationCause,
        inputs: Vec<WorkflowNodeInput>,
    ) -> WorkflowGraphJournalEvent {
        let node = activation_node(node);
        let iteration = state
            .node(&node)
            .expect("projected activation node")
            .iteration
            .saturating_add(1);
        WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
            graph_id: state.graph_id.clone(),
            node,
            iteration,
            activation_order: state.next_activation_order,
            input_ledger_digest: workflow_input_ledger_digest(&inputs),
            inputs,
            cause,
        })
    }

    fn completion_event(
        state: &WorkflowGraphState,
        node: &str,
        output: InstructEvidenceRef,
    ) -> WorkflowGraphJournalEvent {
        let node = activation_node(node);
        let iteration = state
            .node(&node)
            .expect("projected completion node")
            .iteration;
        let outputs = vec![output];
        WorkflowGraphJournalEvent::WorkflowNodeCompleted(WorkflowNodeCompleted {
            graph_id: state.graph_id.clone(),
            node,
            iteration,
            output_ledger_digest: workflow_evidence_ledger_digest(&outputs),
            outputs,
            convergence: None,
        })
    }

    #[test]
    fn activation_requires_typed_inputs_and_join_waits_for_every_branch() {
        let mut state = started_activation_state(diamond_activation_ast());
        let seed = activation_seed(&state);
        let substituted_seed = InstructEvidenceRef::new(
            seed.artifact.clone(),
            seed.evidence_type.clone(),
            seed.byte_len.saturating_add(1),
            Vec::new(),
        );
        let substituted_activation = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: substituted_seed,
            }],
        );
        assert!(
            state.apply(2, &substituted_activation).is_err(),
            "a valid ledger row with the same address cannot replace the exact seed evidence"
        );
        let root_activation = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: seed.clone(),
            }],
        );
        state.apply(2, &root_activation).expect("root activates");
        let root_output = activation_evidence('b', "RootFact", vec![seed.artifact.clone()]);
        let root_completion = completion_event(&state, "ROOT", root_output.clone());
        state.apply(3, &root_completion).expect("root completes");
        let duplicate_root = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: seed,
            }],
        );
        assert!(
            state.apply(4, &duplicate_root).is_err(),
            "a completed node needs an explicit back-edge reset before reactivation"
        );

        for (cursor, node, edge) in [(4, "LEFT", 2), (5, "RIGHT", 3)] {
            let event = activation_event(
                &state,
                node,
                WorkflowActivationCause::ForwardJoin,
                vec![WorkflowNodeInput {
                    edge_id: edge,
                    evidence: root_output.clone(),
                }],
            );
            state.apply(cursor, &event).expect("branch activates");
        }
        let left_output = activation_evidence('c', "LeftFact", vec![root_output.artifact.clone()]);
        let left_completion = completion_event(&state, "LEFT", left_output.clone());
        state.apply(6, &left_completion).expect("left completes");

        let absent_right =
            activation_evidence('d', "RightFact", vec![root_output.artifact.clone()]);
        let premature_join = activation_event(
            &state,
            "JOIN",
            WorkflowActivationCause::ForwardJoin,
            vec![
                WorkflowNodeInput {
                    edge_id: 4,
                    evidence: left_output.clone(),
                },
                WorkflowNodeInput {
                    edge_id: 5,
                    evidence: absent_right,
                },
            ],
        );
        assert!(
            state.apply(7, &premature_join).is_err(),
            "a typed artifact not completed by its upstream branch cannot activate the join"
        );

        let right_output =
            activation_evidence('e', "RightFact", vec![root_output.artifact.clone()]);
        let right_completion = completion_event(&state, "RIGHT", right_output.clone());
        state
            .apply(7, &right_completion)
            .expect("right completes after rejected mutation");
        let complete_join = activation_event(
            &state,
            "JOIN",
            WorkflowActivationCause::ForwardJoin,
            vec![
                WorkflowNodeInput {
                    edge_id: 4,
                    evidence: left_output,
                },
                WorkflowNodeInput {
                    edge_id: 5,
                    evidence: right_output,
                },
            ],
        );
        state
            .apply(8, &complete_join)
            .expect("join activates only after both branches complete");
    }

    #[test]
    fn terminal_rejection_seals_waiting_sibling_activations() {
        let mut state = started_activation_state(diamond_activation_ast());
        let seed = activation_seed(&state);
        let root_activation = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: seed.clone(),
            }],
        );
        state.apply(2, &root_activation).expect("root activates");
        let root_output = activation_evidence('b', "RootFact", vec![seed.artifact]);
        let root_completion = completion_event(&state, "ROOT", root_output.clone());
        state.apply(3, &root_completion).expect("root completes");

        let left_activation = activation_event(
            &state,
            "LEFT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 2,
                evidence: root_output.clone(),
            }],
        );
        state.apply(4, &left_activation).expect("left activates");
        let abandoned = WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
            graph_id: state.graph_id.clone(),
            node: activation_node("LEFT"),
            iteration: 1,
            code: WorkflowNodeRejectCode::Abandoned,
            message: "workflow graph was abandoned".into(),
            evidence: None,
            convergence_gate: false,
        });
        state.apply(5, &abandoned).expect("graph is abandoned");

        let forged_sibling = activation_event(
            &state,
            "RIGHT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 3,
                evidence: root_output,
            }],
        );
        assert!(
            state.apply(6, &forged_sibling).is_err(),
            "a terminal graph rejection seals every unfinished sibling"
        );
    }

    #[test]
    fn activation_ast_rechecks_node_inputs_and_merged_terminal_outputs() {
        let mut invalid = diamond_activation_ast();
        invalid.nodes[1].input_type = "DifferentFact".into();
        assert!(validate_workflow_activation_ast(&invalid).is_err());

        let mut zero_edge = diamond_activation_ast();
        zero_edge.edges[0].id = 0;
        zero_edge.nodes[0].join.initial_all[0] = 0;
        assert!(validate_workflow_activation_ast(&zero_edge).is_err());

        let mut duplicate_graph_input = diamond_activation_ast();
        duplicate_graph_input.edges.push(WorkflowActivationEdge {
            id: 6,
            kind: WorkflowEdgeKind::GraphInput,
            from: None,
            to: activation_node("ROOT"),
            evidence_type: "Question".into(),
        });
        duplicate_graph_input.nodes[0].join.initial_all.push(6);
        assert!(validate_workflow_activation_ast(&duplicate_graph_input).is_err());

        let mut forward_disguised_as_back = diamond_activation_ast();
        forward_disguised_as_back
            .edges
            .push(WorkflowActivationEdge {
                id: 6,
                kind: WorkflowEdgeKind::Back,
                from: Some(activation_node("ROOT")),
                to: activation_node("JOIN"),
                evidence_type: "LeftFact + RightFact".into(),
            });
        forward_disguised_as_back.nodes[3]
            .join
            .reactivate_any
            .push(6);
        assert!(validate_workflow_activation_ast(&forward_disguised_as_back).is_err());

        let mut sibling_disguised_as_back = diamond_activation_ast();
        sibling_disguised_as_back
            .edges
            .push(WorkflowActivationEdge {
                id: 6,
                kind: WorkflowEdgeKind::Back,
                from: Some(activation_node("RIGHT")),
                to: activation_node("LEFT"),
                evidence_type: "RootFact".into(),
            });
        sibling_disguised_as_back.nodes[1]
            .join
            .reactivate_any
            .push(6);
        assert!(validate_workflow_activation_ast(&sibling_disguised_as_back).is_err());

        for invalid_type in ["   ", "Question++Injected", "snowman-☃"] {
            let mut invalid_type_ast = diamond_activation_ast();
            invalid_type_ast.nodes[0].input_type = invalid_type.into();
            assert!(validate_workflow_activation_ast(&invalid_type_ast).is_err());
        }

        let pipe = crate::loom::parse_pipe(
            "terminals-runtime: Seed -> Right + Left\nstart\nleft @left <-start\nright @right <-start",
        );
        let workflow = crate::loom::compile_pipe(&pipe, |id: &str| match id {
            "left" => Some(crate::loom::LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Left".into(),
            }),
            "right" => Some(crate::loom::LoomTypeSig {
                in_type: "Seed".into(),
                out_type: "Right".into(),
            }),
            _ => None,
        })
        .expect("multi-terminal workflow compiles");
        let ast = workflow_activation_ast_from_loom(&workflow)
            .expect("multi-terminal workflow lowers to a valid activation AST");
        assert_eq!(ast.output_type, "Right + Left");
        assert!(!ast.nodes[0].convergence_gate);
        assert!(ast.nodes[1..].iter().all(|node| node.convergence_gate));
    }

    fn loop_activation_ast() -> WorkflowActivationAst {
        let root = activation_node("ROOT");
        WorkflowActivationAst {
            workflow_id: "activation-loop".into(),
            workflow_digest: "blake3:compiled-loop".into(),
            input_type: "Question".into(),
            output_type: "Answer".into(),
            nodes: vec![WorkflowActivationNode {
                node: root.clone(),
                input_type: "Question".into(),
                output_type: "Answer".into(),
                join: WorkflowJoinSemantics {
                    initial_all: vec![1],
                    reactivate_any: vec![2],
                },
                convergence_gate: false,
            }],
            edges: vec![
                WorkflowActivationEdge {
                    id: 1,
                    kind: WorkflowEdgeKind::GraphInput,
                    from: None,
                    to: root.clone(),
                    evidence_type: "Question".into(),
                },
                WorkflowActivationEdge {
                    id: 2,
                    kind: WorkflowEdgeKind::Back,
                    from: Some(root.clone()),
                    to: root,
                    evidence_type: "Question".into(),
                },
            ],
            max_back_edge_activations: 1,
        }
    }

    fn reject_loop_iteration(
        state: &WorkflowGraphState,
        marker: char,
    ) -> WorkflowGraphJournalEvent {
        let root = activation_node("ROOT");
        let node = state.node(&root).expect("loop node");
        let parents = node
            .inputs
            .iter()
            .map(|input| input.evidence.artifact.clone())
            .collect();
        WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
            graph_id: state.graph_id.clone(),
            node: root,
            iteration: node.iteration,
            code: WorkflowNodeRejectCode::EvidenceRejected,
            message: "loop evidence was rejected".into(),
            evidence: Some(activation_evidence(marker, "Question", parents)),
            convergence_gate: false,
        })
    }

    #[test]
    fn back_edge_reactivates_once_and_bounded_guard_records_the_next_rejection() {
        let mut state = started_activation_state(loop_activation_ast());
        let first = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: activation_seed(&state),
            }],
        );
        state.apply(2, &first).expect("first activation");
        let first_reject = reject_loop_iteration(&state, 'b');
        state.apply(3, &first_reject).expect("first reject");
        let back_input = state
            .node(&activation_node("ROOT"))
            .and_then(|node| node.rejection.as_ref())
            .and_then(|reject| reject.evidence.clone())
            .expect("back evidence");
        let second = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::BackEdge,
            vec![WorkflowNodeInput {
                edge_id: 2,
                evidence: back_input,
            }],
        );
        state.apply(4, &second).expect("bounded reactivation");
        assert_eq!(state.back_edge_activations, 1);
        assert_eq!(
            state
                .node(&activation_node("ROOT"))
                .expect("root")
                .iteration,
            2
        );

        let second_reject = reject_loop_iteration(&state, 'c');
        state.apply(5, &second_reject).expect("second reject");
        let next_input = state
            .node(&activation_node("ROOT"))
            .and_then(|node| node.rejection.as_ref())
            .and_then(|reject| reject.evidence.clone())
            .expect("next back evidence");
        let over_guard = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::BackEdge,
            vec![WorkflowNodeInput {
                edge_id: 2,
                evidence: next_input,
            }],
        );
        let guard_rejection =
            WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
                graph_id: state.graph_id.clone(),
                node: activation_node("ROOT"),
                iteration: 3,
                code: WorkflowNodeRejectCode::IterationGuard,
                message: "bounded back-edge guard exhausted".into(),
                evidence: None,
                convergence_gate: false,
            });
        state
            .apply(6, &guard_rejection)
            .expect("guard rejection is durable state");
        assert_eq!(
            state
                .node(&activation_node("ROOT"))
                .and_then(|node| node.rejection.as_ref())
                .map(|rejection| rejection.code),
            Some(WorkflowNodeRejectCode::IterationGuard)
        );
        assert!(state.apply(7, &over_guard).is_err());
    }

    #[test]
    fn back_edge_cannot_reopen_a_graph_that_never_received_external_input() {
        let ast = loop_activation_ast();
        let mut state = WorkflowGraphState::from_started(
            1,
            WorkflowGraphStarted {
                graph_id: GraphId::new("activation-without-input"),
                ast_digest: workflow_activation_ast_digest(&ast),
                ast,
                seed: None,
            },
        )
        .expect("waiting graph");
        let evidence = activation_evidence('b', "Question", Vec::new());
        state
            .apply(
                2,
                &WorkflowGraphJournalEvent::WorkflowNodeRejected(WorkflowNodeRejected {
                    graph_id: state.graph_id.clone(),
                    node: activation_node("ROOT"),
                    iteration: 1,
                    code: WorkflowNodeRejectCode::Abandoned,
                    message: "abandoned before input".into(),
                    evidence: Some(evidence.clone()),
                    convergence_gate: false,
                }),
            )
            .expect("terminal pre-input rejection remains inspectable");
        let inputs = vec![WorkflowNodeInput {
            edge_id: 2,
            evidence,
        }];
        let replay_mutation =
            WorkflowGraphJournalEvent::WorkflowNodeActivated(WorkflowNodeActivated {
                graph_id: state.graph_id.clone(),
                node: activation_node("ROOT"),
                iteration: 2,
                activation_order: 1,
                cause: WorkflowActivationCause::BackEdge,
                input_ledger_digest: workflow_input_ledger_digest(&inputs),
                inputs,
            });
        assert!(state.apply(3, &replay_mutation).is_err());
        state
            .validate_projection()
            .expect("the rejected no-input projection stays internally valid");
    }

    #[test]
    fn convergence_gate_requires_a_stamp_or_an_inspectable_gate_rejection() {
        let mut ast = loop_activation_ast();
        ast.nodes[0].convergence_gate = true;
        let mut state = started_activation_state(ast);
        let activation = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: activation_seed(&state),
            }],
        );
        state.apply(2, &activation).expect("gate activates");
        let output = activation_evidence('b', "Answer", vec![activation_seed(&state).artifact]);
        let unstamped = completion_event(&state, "ROOT", output.clone());
        assert!(state.apply(3, &unstamped).is_err());

        let mut completed_state = state.clone();
        let mut stamped = completion_event(&completed_state, "ROOT", output);
        let WorkflowGraphJournalEvent::WorkflowNodeCompleted(completed) = &mut stamped else {
            unreachable!("completion helper always constructs a completion")
        };
        completed.convergence = Some(WorkflowConvergenceStamp {
            decision_digest: completed.outputs[0].ledger_digest.clone(),
        });
        completed_state
            .apply(3, &stamped)
            .expect("stamped convergence completion");
        assert!(
            completed_state
                .node(&activation_node("ROOT"))
                .and_then(|node| node.convergence.as_ref())
                .is_some(),
            "the state RPC projection retains the inspectable convergence stamp"
        );

        let mut rejection = WorkflowNodeRejected {
            graph_id: state.graph_id.clone(),
            node: activation_node("ROOT"),
            iteration: 1,
            code: WorkflowNodeRejectCode::ConvergenceRejected,
            message: "loop evidence was rejected".into(),
            evidence: Some(activation_evidence(
                'c',
                "Question",
                vec![activation_seed(&state).artifact],
            )),
            convergence_gate: false,
        };
        let rejected = WorkflowGraphJournalEvent::WorkflowNodeRejected(rejection.clone());
        assert!(state.apply(3, &rejected).is_err());
        rejection.convergence_gate = true;
        let rejected = WorkflowGraphJournalEvent::WorkflowNodeRejected(rejection);
        state
            .apply(3, &rejected)
            .expect("gate rejection retains typed detail");
        assert_eq!(
            state
                .node(&activation_node("ROOT"))
                .and_then(|node| node.rejection.as_ref())
                .map(|rejection| rejection.message.as_str()),
            Some("loop evidence was rejected")
        );
    }

    fn activation_fact(seq: u64, event: WorkflowGraphJournalEvent) -> RawEnvelope {
        let mut envelope = graph_fact(seq, EventPayload::IdleDecayed);
        envelope.payload = event.to_payload_value().expect("activation payload");
        envelope
    }

    #[test]
    fn replay_reproduces_activation_order_and_rejects_an_order_mutation() {
        let ast = loop_activation_ast();
        let started = WorkflowGraphStarted {
            graph_id: GraphId::new("activation-graph"),
            ast_digest: workflow_activation_ast_digest(&ast),
            seed: Some(activation_evidence('a', &ast.input_type, Vec::new())),
            ast,
        };
        let start_event =
            WorkflowGraphJournalEvent::WorkflowGraphStarted(Box::new(started.clone()));
        let state = WorkflowGraphState::from_started(1, started).expect("replay start");
        let activation = activation_event(
            &state,
            "ROOT",
            WorkflowActivationCause::ForwardJoin,
            vec![WorkflowNodeInput {
                edge_id: 1,
                evidence: activation_seed(&state),
            }],
        );
        let journal = vec![
            activation_fact(1, start_event),
            activation_fact(2, activation.clone()),
        ];
        let replayed = reduce_workflow_graphs(&journal).expect("deterministic replay");
        assert_eq!(
            replayed[&GraphId::new("activation-graph")].activation_order[0].cursor,
            2
        );
        replayed[&GraphId::new("activation-graph")]
            .validate_projection()
            .expect("replayed state validates as an indexed projection");
        let mut projection_mutation = replayed[&GraphId::new("activation-graph")].clone();
        projection_mutation.next_activation_order =
            projection_mutation.next_activation_order.saturating_add(1);
        assert!(projection_mutation.validate_projection().is_err());

        let mut mutation = journal;
        mutation[1].payload["activation_order"] = serde_json::json!(2);
        assert!(reduce_workflow_graphs(&mutation).is_err());
    }
}
