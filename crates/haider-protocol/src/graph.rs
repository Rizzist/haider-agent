//! Convergence Graph M1 contracts and pure journal reduction.
//!
//! A graph is an immutable template instance. Executors never advance it:
//! they append [`EvidenceRecorded`] facts and the daemon alone appends gate,
//! advancement, retry, blocking, and completion facts. M2a adds declared
//! replacement slots and daemon-recorded process signals while retaining the
//! exact flat-counter branch for legacy pinned instances.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::ids::{ArtifactRef, EffectId, GraphId, MenuId, RunId, WorkspaceRevision};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

pub const SHIP_LOOP_TEMPLATE: &str = "ship-loop";
pub const SUPER_SHIP_LOOP_TEMPLATE: &str = "super-ship-loop";
pub const STAGGERED_TEMPLATE: &str = "staggered";
pub const SEC_AUDIT_TEMPLATE: &str = "sec-audit";
pub const DOCS_SWEEP_TEMPLATE: &str = "docs-sweep";
pub const GRAPH_TEMPLATE_VERSION: u32 = 1;
pub const GRAPH_NODE_NAME_MAX_BYTES: usize = 64;
pub const GRAPH_MAX_NODES: usize = 512;
pub const GRAPH_MAX_EDGES: usize = 4_096;
pub const GRAPH_MAX_SLOTS: usize = 4_096;
pub const GRAPH_MAX_ATTEMPTS: u32 = 8;
pub const GRAPH_MAX_EVIDENCE_PER_ATTEMPT: u32 = 8;
pub const GRAPH_EVIDENCE_DETAIL_MAX_BYTES: usize = 1_024;
pub const GRAPH_BRIEF_MAX_BYTES: usize = 400;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// red and opens the next BUILD epoch. Human gates have no such round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_evidence_per_attempt: Option<u32>,
    /// Immutable incoming dependencies. M1's built-in template is a simple
    /// linear DAG, but the dependency shape is stamped rather than inferred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<GraphNodeName>,
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
    /// M1's graph-wide attempt epoch. Reopening BUILD increments it and every
    /// later node in that lineage carries the same value.
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
}

/// Stable coordinates submitted by `graph_evidence` for daemon-verified
/// process truth. The daemon resolves all three fields against the journal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessSignalRef {
    pub run_id: RunId,
    pub call_id: String,
    pub effect_id: EffectId,
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
    /// The graph-wide BUILD lineage epoch, implied by the open obligation at
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
        let mut line = format!(
            "GraphBrief: {} attempt {}/{}; graph_id={}; ready={}; gate {}; evidence {} green/{} red ({} effective); next: {}.",
            node.label(),
            self.attempt,
            GRAPH_MAX_ATTEMPTS,
            self.graph_id,
            ready,
            gate,
            node_status.evidence.green,
            node_status.evidence.red,
            node_status.evidence.effective_green,
            expectation,
        );
        truncate_utf8(&mut line, GRAPH_BRIEF_MAX_BYTES);
        Some(line)
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphReduction {
    pub status: Option<GraphStatus>,
    pub evidence: Vec<EvidenceRecorded>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphReductions {
    pub active_root: Option<GraphId>,
    pub by_graph: HashMap<GraphId, GraphReduction>,
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
        let Some(payload) = graph_reduction_payload(&envelope.payload) else {
            continue;
        };
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
                };
                reductions.by_graph.insert(
                    graph_id.clone(),
                    GraphReduction {
                        status: Some(status),
                        evidence: Vec::new(),
                        template_nodes,
                    },
                );
                reductions.active_root = Some(graph_id);
            }
            EventPayload::GraphAttemptOpened(opened) => {
                let Some(reduction) = reductions.by_graph.get_mut(&opened.graph_id) else {
                    continue;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&opened.graph_id) else {
                    continue;
                };
                if status.phase != GraphPhase::Active {
                    continue;
                }
                status.attempt = opened.attempt;
                let start_node = status.start_node.clone().unwrap_or_else(build_node);
                if opened.node == start_node {
                    // A new START opening is a new graph-wide revision epoch:
                    // every prior gate/evidence projection is stale, while
                    // immutable attempt counts remain historical truth.
                    for node in &mut status.nodes {
                        node.current_attempt = None;
                        node.clear_evidence_frontier();
                        node.satisfied = false;
                    }
                    status.ready_nodes.clear();
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
                let Some(reduction) = reductions.by_graph.get_mut(&recorded.graph_id) else {
                    continue;
                };
                let Some(status) = reduction.status.as_mut() else {
                    continue;
                };
                if status.phase != GraphPhase::Active
                    || !status.node_is_ready(&recorded.node)
                    || status.attempt != recorded.attempt
                {
                    continue;
                }
                if let Some(node) = status.node_mut(&recorded.node) {
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
                let Some(reduction) = reductions.by_graph.get_mut(&satisfied.graph_id) else {
                    continue;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&satisfied.graph_id) else {
                    continue;
                };
                if let Some(node) = status.node_mut(&satisfied.node) {
                    node.satisfied = true;
                }
                if status.template_version > 0 {
                    status.ready_nodes.retain(|node| node != &satisfied.node);
                    refresh_current_node(status, &template_nodes);
                }
            }
            EventPayload::GraphAdvanced(advanced) => {
                if let Some(status) = reductions
                    .by_graph
                    .get_mut(&advanced.graph_id)
                    .and_then(|reduction| reduction.status_for_graph_mut(&advanced.graph_id))
                    && status.template_version == 0
                {
                    status.current_node = Some(advanced.to_node);
                }
            }
            EventPayload::GraphNodeReadied(readied) => {
                let Some(reduction) = reductions.by_graph.get_mut(&readied.graph_id) else {
                    continue;
                };
                let template_nodes = reduction.template_nodes.clone();
                let Some(status) = reduction.status_for_graph_mut(&readied.graph_id) else {
                    continue;
                };
                if status.phase == GraphPhase::Active && status.attempt == readied.attempt {
                    push_ready_in_template_order(status, &template_nodes, readied.node);
                }
            }
            EventPayload::GraphBlocked(blocked) => {
                if let Some(status) = reductions
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
                if let Some(status) = reductions
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
                if let Some(status) = reductions
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
            EventPayload::GraphSuperseded(superseded) => {
                if let Some(status) = reductions
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
                reductions.active_root = Some(superseded.new);
            }
            EventPayload::MenuOpened(menu) => {
                if let crate::menu::MenuKind::GraphHumanConfirm { graph_id, .. } = &menu.kind
                    && let Some(status) = reductions
                        .by_graph
                        .get_mut(graph_id)
                        .and_then(|reduction| reduction.status_for_graph_mut(graph_id))
                {
                    status.pending_menu.get_or_insert_with(|| menu.id.clone());
                    if status.template_version > 0
                        && !status.pending_menus.iter().any(|id| id == &menu.id)
                    {
                        status.pending_menus.push(menu.id);
                    }
                }
            }
            EventPayload::MenuAnswered(crate::menu::MenuAnswer { menu, .. })
            | EventPayload::MenuClosed { menu, .. } => {
                for reduction in reductions.by_graph.values_mut() {
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
    reductions
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

fn graph_reduction_payload(payload: &serde_json::Value) -> Option<EventPayload> {
    let kind = payload.get("type")?.as_str()?;
    if !kind.starts_with("graph_") && kind != "evidence_recorded" && !kind.starts_with("menu_") {
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
            verify_slots: Vec::new(),
        },
        GraphNodeSpec {
            name: verify_node(),
            gate: GraphGateKind::AllOfN { n: 3 },
            executor: GraphExecutorShape::FanOut,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
            depends_on: vec![build_node()],
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
    vec![
        ship_loop_template(),
        super_ship_loop_template(),
        staggered_template(),
        sec_audit_template(),
        docs_sweep_template(),
    ]
}

#[must_use]
pub fn graph_template(name: &str) -> Option<GraphTemplateSpec> {
    graph_template_catalog()
        .into_iter()
        .find(|template| template.name == name)
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
    template(
        SUPER_SHIP_LOOP_TEMPLATE,
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
                "TESTS",
                GraphGateKind::AllOfN { n: 2 },
                GraphExecutorShape::FanOut,
                &["START"],
                vec![daemon_slot("tests"), daemon_slot("lint")],
            ),
            node_spec(
                "REVIEW",
                GraphGateKind::AllOfN { n: 1 },
                GraphExecutorShape::Inline,
                &["START"],
                vec![model_slot("review")],
            ),
            node_spec(
                "PACKAGE",
                GraphGateKind::CommandGreen,
                GraphExecutorShape::Inline,
                &["TESTS", "REVIEW"],
                vec![],
            ),
            node_spec(
                "SHIP",
                GraphGateKind::HumanConfirm,
                GraphExecutorShape::Human,
                &["PACKAGE"],
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
mod tests {
    use super::*;

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
    }
}
