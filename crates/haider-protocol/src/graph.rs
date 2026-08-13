//! Convergence Graph M1 contracts and pure journal reduction.
//!
//! A graph is an immutable template instance. Executors never advance it:
//! they append [`EvidenceRecorded`] facts and the daemon alone appends gate,
//! advancement, retry, blocking, and completion facts. M1 deliberately has
//! no child-graph attachment and does not harvest child reports; the root
//! model reads ordinary child results and explicitly testifies through the
//! `graph_evidence` tool.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::ids::{GraphId, MenuId, RunId};
use serde::{Deserialize, Serialize};

pub const SHIP_LOOP_TEMPLATE: &str = "ship-loop";
pub const GRAPH_MAX_ATTEMPTS: u32 = 8;
pub const GRAPH_MAX_EVIDENCE_PER_ATTEMPT: u32 = 8;
pub const GRAPH_EVIDENCE_DETAIL_MAX_BYTES: usize = 1_024;
pub const GRAPH_BRIEF_MAX_BYTES: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GraphNodeName {
    Build,
    Verify,
    Ship,
}

impl GraphNodeName {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Build => "BUILD",
            Self::Verify => "VERIFY",
            Self::Ship => "SHIP",
        }
    }
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphPinned {
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
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
    Model { run_id: RunId, call_id: String },
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeStatus {
    pub node: GraphNodeName,
    pub attempts_opened: u32,
    pub current_attempt: Option<u32>,
    pub evidence: GraphEvidenceTally,
    pub satisfied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStatus {
    pub graph_id: GraphId,
    pub template: String,
    pub digest: String,
    pub phase: GraphPhase,
    pub current_node: Option<GraphNodeName>,
    pub attempt: u32,
    pub nodes: Vec<GraphNodeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GraphBlockReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_menu: Option<MenuId>,
}

impl GraphStatus {
    #[must_use]
    pub fn accepts_evidence(&self) -> bool {
        self.phase == GraphPhase::Active
            && matches!(
                self.current_node,
                Some(GraphNodeName::Build | GraphNodeName::Verify)
            )
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
        let node = self.current_node?;
        let node_status = self.nodes.iter().find(|status| status.node == node)?;
        let gate = match node {
            GraphNodeName::Build => "command-green",
            GraphNodeName::Verify => "all-of-3",
            GraphNodeName::Ship => "human-confirm",
        };
        let expectation = match (self.phase, node) {
            (GraphPhase::Blocked, _) => "re-pin or abandon",
            (_, GraphNodeName::Build) => "record BUILD evidence",
            (_, GraphNodeName::Verify) => "record 3 green VERIFY results",
            (_, GraphNodeName::Ship) => "await explicit human confirm",
        };
        let mut line = format!(
            "GraphBrief: {} attempt {}/{}; gate {}; evidence {} green/{} red ({} effective); next: {}.",
            node.label(),
            self.attempt,
            GRAPH_MAX_ATTEMPTS,
            gate,
            node_status.evidence.green,
            node_status.evidence.red,
            node_status.evidence.effective_green,
            expectation,
        );
        truncate_utf8(&mut line, GRAPH_BRIEF_MAX_BYTES);
        Some(line)
    }

    fn node_mut(&mut self, node: GraphNodeName) -> Option<&mut GraphNodeStatus> {
        self.nodes.iter_mut().find(|status| status.node == node)
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

/// Reduces the latest graph instance from session journal truth. Unknown
/// payloads and graph facts for older instances remain tolerated.
#[must_use]
pub fn reduce_graph(envelopes: &[RawEnvelope]) -> GraphReduction {
    let mut reduction = GraphReduction::default();
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
                    nodes: template_nodes,
                } = pinned;
                reduction.evidence.clear();
                reduction.status = Some(GraphStatus {
                    graph_id,
                    template,
                    digest,
                    phase: GraphPhase::Active,
                    current_node: None,
                    attempt: 0,
                    nodes: template_nodes
                        .iter()
                        .map(|spec| GraphNodeStatus {
                            node: spec.name,
                            attempts_opened: 0,
                            current_attempt: None,
                            evidence: GraphEvidenceTally::default(),
                            satisfied: false,
                        })
                        .collect(),
                    blocked_reason: None,
                    pending_menu: None,
                });
                reduction.template_nodes = template_nodes;
            }
            EventPayload::GraphAttemptOpened(opened) => {
                let Some(status) = reduction.status_for_graph_mut(&opened.graph_id) else {
                    continue;
                };
                if status.phase != GraphPhase::Active {
                    continue;
                }
                status.current_node = Some(opened.node);
                status.attempt = opened.attempt;
                if opened.node == GraphNodeName::Build {
                    // A new BUILD opening is a new graph-wide revision epoch:
                    // every prior gate/evidence projection is stale, while
                    // immutable attempt counts remain historical truth.
                    for node in &mut status.nodes {
                        node.current_attempt = None;
                        node.evidence = GraphEvidenceTally::default();
                        node.satisfied = false;
                    }
                }
                if let Some(node) = status.node_mut(opened.node) {
                    node.attempts_opened = node.attempts_opened.saturating_add(1);
                    node.current_attempt = Some(opened.attempt);
                    node.evidence = GraphEvidenceTally::default();
                    node.satisfied = false;
                }
            }
            EventPayload::EvidenceRecorded(recorded) => {
                let Some(status) = reduction.status_for_graph_mut(&recorded.graph_id) else {
                    continue;
                };
                if status.phase != GraphPhase::Active
                    || status.current_node != Some(recorded.node)
                    || status.attempt != recorded.attempt
                {
                    continue;
                }
                if let Some(node) = status.node_mut(recorded.node) {
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
                }
                reduction.evidence.push(recorded);
            }
            EventPayload::GraphGateSatisfied(satisfied) => {
                let Some(status) = reduction.status_for_graph_mut(&satisfied.graph_id) else {
                    continue;
                };
                if let Some(node) = status.node_mut(satisfied.node) {
                    node.satisfied = true;
                }
            }
            EventPayload::GraphAdvanced(advanced) => {
                if let Some(status) = reduction.status_for_graph_mut(&advanced.graph_id) {
                    status.current_node = Some(advanced.to_node);
                }
            }
            EventPayload::GraphBlocked(blocked) => {
                if let Some(status) = reduction.status_for_graph_mut(&blocked.graph_id) {
                    status.phase = GraphPhase::Blocked;
                    status.current_node = Some(blocked.node);
                    status.blocked_reason = Some(blocked.reason);
                    status.pending_menu = None;
                }
            }
            EventPayload::GraphCompleted(completed) => {
                if let Some(status) = reduction.status_for_graph_mut(&completed.graph_id) {
                    status.phase = GraphPhase::Completed;
                    status.pending_menu = None;
                }
            }
            EventPayload::GraphAbandoned(abandoned) => {
                if let Some(status) = reduction.status_for_graph_mut(&abandoned.graph_id) {
                    status.phase = GraphPhase::Abandoned;
                    status.pending_menu = None;
                }
            }
            EventPayload::MenuOpened(menu) => {
                if let crate::menu::MenuKind::GraphHumanConfirm { graph_id, .. } = &menu.kind
                    && let Some(status) = reduction.status_for_graph_mut(graph_id)
                {
                    status.pending_menu = Some(menu.id);
                }
            }
            EventPayload::MenuAnswered(crate::menu::MenuAnswer { menu, .. })
            | EventPayload::MenuClosed { menu, .. } => {
                if let Some(status) = reduction.status.as_mut()
                    && status.pending_menu.as_ref() == Some(&menu)
                {
                    status.pending_menu = None;
                }
            }
            _ => {}
        }
    }
    reduction
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
            name: GraphNodeName::Build,
            gate: GraphGateKind::CommandGreen,
            executor: GraphExecutorShape::Inline,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
            depends_on: Vec::new(),
        },
        GraphNodeSpec {
            name: GraphNodeName::Verify,
            gate: GraphGateKind::AllOfN { n: 3 },
            executor: GraphExecutorShape::FanOut,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: Some(GRAPH_MAX_EVIDENCE_PER_ATTEMPT),
            depends_on: vec![GraphNodeName::Build],
        },
        GraphNodeSpec {
            name: GraphNodeName::Ship,
            gate: GraphGateKind::HumanConfirm,
            executor: GraphExecutorShape::Human,
            max_attempts: GRAPH_MAX_ATTEMPTS,
            max_evidence_per_attempt: None,
            depends_on: vec![GraphNodeName::Verify],
        },
    ]
}

#[must_use]
pub fn ship_loop_digest() -> String {
    // The name and every executable bound are part of immutable template
    // identity. A semantic template edit must mint a different digest.
    let bytes = serde_json::to_vec(&(SHIP_LOOP_TEMPLATE, ship_loop_nodes())).unwrap_or_default();
    blake3::hash(&bytes).to_hex().to_string()
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
            phase: GraphPhase::Active,
            current_node: Some(GraphNodeName::Verify),
            attempt: 2,
            nodes: ship_loop_nodes()
                .into_iter()
                .map(|node| GraphNodeStatus {
                    node: node.name,
                    attempts_opened: 1,
                    current_attempt: Some(2),
                    evidence: GraphEvidenceTally::default(),
                    satisfied: false,
                })
                .collect(),
            blocked_reason: None,
            pending_menu: None,
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
}
