//! Agents and delegation (§5). Haider executes agents locally; the historical
//! device-shaped wire variant is retained only to reject it cleanly when
//! reading a future/stale manifest. Callsigns are display identity ONLY.

use crate::credential::AuthMethod;
use crate::ids::{AgentId, DeviceId, LeaseId, RunId, SessionId, WorkspaceRevision};
use crate::provider::UsageRequestKind;
use crate::state::RunState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent: AgentId,
    pub role: AgentRole,
    /// Short, persisted display label for the delegated task. This is
    /// presentation metadata only; operational routing remains keyed by the
    /// opaque [`AgentId`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    /// Display-only callsign (honour-roll or neutral roster). Never in logs,
    /// addresses, metrics, or failure strings (§5.1 dignity rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    pub model_profile: String,
    /// Capability grant: subdivides from the parent, never grows.
    pub grant: Grant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    pub placement: Placement,
    pub lease: LeaseId,
    pub fencing_epoch: u64,
    /// Attempt number for this manifest (stall-kill/respawn increments it).
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<AgentId>,
    /// Reserved: workflow/run coordinates (ADW lands post-0.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinates: Option<serde_json::Value>,
    /// B3 (Loom) — the typed child's declared-CLI exec scope, FROZEN at
    /// spawn (durable manifest truth): later registry edits never widen a
    /// running child's executable set. `None` = untyped/unfenced;
    /// `Some(vec![])` = typed deny-all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_scope: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Head,
    Subagent,
    /// Spawns and steers, never codes — effect policy strips mutating tools.
    Orchestrator,
}

/// v0.1: local only. Remote placement fields reserved for W-post-0.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "placement", rename_all = "snake_case")]
pub enum Placement {
    Local,
    /// Reserved wire compatibility only. Production admission rejects it.
    Device {
        device: DeviceId,
    },
}

impl Placement {
    /// Enforces the owner-scoped local-only product boundary.
    pub fn ensure_local(&self) -> Result<(), crate::error::HaiderError> {
        if matches!(self, Self::Local) {
            return Ok(());
        }
        Err(crate::error::HaiderError::new(
            crate::error::ErrorCode::InvalidArgument,
            "not supported — Haider runs local-only",
            false,
        )
        .with_presentation(crate::error::ErrorPresentation::new(
            "local-only",
            "Not supported — Haider runs local-only",
            "Cross-device agent placement is not available in Haider.",
            crate::error::ErrorScope::Tool,
            [crate::error::ErrorAction::None],
        )))
    }
}

/// The capability grant carried by a manifest. Coarse in v0.1 (tool allowlist
/// + effect ceiling); narrows per §8 gates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub tools: Vec<String>,
    /// Effects the agent may even *request* (asking still gates each call).
    pub effect_ceiling: Vec<crate::effect::EffectClass>,
}

/// Subagent display state for chips (projection of its own run states).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChipState {
    Idle,
    Thinking,
    Streaming,
    Tool,
    Waiting,
    InputRequired,
    PermissionRequired,
    Done,
    Error,
    Closed,
}

/// A child's terminal report to its parent (collect step).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildReport {
    pub agent: AgentId,
    pub summary: String,
    /// Verification status the report carries to the parent (§9.2).
    pub verified: ReportVerification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportVerification {
    Verified,
    Red,
    Waived,
    Unverified,
}

/// How one parent-authored child message crossed the worker boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageDelivery {
    DeliveredSteer,
    DeliveredQueued,
    DeliveredSubturn,
}

/// Receipt returned by both the model tool and the chip-composer wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMessageReceipt {
    pub agent: AgentId,
    pub delivery: AgentMessageDelivery,
    pub child_run_id: RunId,
    pub child_run_state: RunState,
}

/// Bounded parent-timeline fact for a message sent to a direct child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessaged {
    pub agent: AgentId,
    pub preview: String,
    pub delivery: AgentMessageDelivery,
}

/// Compact direct/exclusive metrics for one agent session at a committed
/// journal head. This is a replace-by-`head_seq` projection: consumers never
/// add snapshots together for the same agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetricsSnapshot {
    /// `None` names the root/head agent; delegated sessions carry their
    /// durable opaque agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    pub session_id: SessionId,
    /// Greatest child-session sequence included in this snapshot.
    pub head_seq: u64,
    pub started_at_ms: u64,
    /// The committed terminal instant. Absent while the totals are partial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at_ms: Option<u64>,
    pub live: bool,
    /// Attempted model tool calls, unique by durable item id.
    pub tool_attempts: u64,
    /// Absent means no durable usage truth, not zero usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsageMetrics>,
}

/// Normalized direct usage and separately-accounted real/API-equivalent
/// costs. Cached reads are already included in `logical_input_tokens`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsageMetrics {
    pub logical_input_tokens: u64,
    pub billed_output_tokens: u64,
    /// Reasoning detail billed in addition to provider-reported output.
    pub additional_reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Present only when the complete logical-input fold has authoritative
    /// cache coverage. 10_000 basis points is 100.00%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_basis_points: Option<u32>,
    /// Real pay-as-you-go spend over API-key lanes only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metered_cost_microusd: Option<u64>,
    /// Hypothetical API-rate equivalent over every known-auth lane. Never
    /// merged into `metered_cost_microusd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_cost_microusd: Option<u64>,
    /// False when any lane has unknown auth or no matching price.
    pub all_lanes_priced: bool,
    pub has_metered_lanes: bool,
    pub has_oauth_lanes: bool,
    #[serde(default)]
    pub breakdowns: Vec<AgentUsageBreakdown>,
}

/// Provider/model/cache-epoch/request-lane detail for an agent snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsageBreakdown {
    pub provider: String,
    pub model: String,
    pub cache_epoch: String,
    #[serde(default)]
    pub request_kind: UsageRequestKind,
    /// Absent is legacy/unknown auth and therefore never rendered as plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<AuthMethod>,
    pub logical_input_tokens: u64,
    pub billed_output_tokens: u64,
    pub additional_reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metered_cost_microusd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_equivalent_cost_microusd: Option<u64>,
    pub priced: bool,
}

/// Additive agent-event union kept separate from [`crate::EventPayload`] so
/// existing exhaustive consumers stay source-compatible. Raw-envelope
/// readers preserve this newer event kind for S3 timeline rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventPayload {
    AgentMessaged(AgentMessaged),
    AgentMetrics(AgentMetricsSnapshot),
}

impl AgentMessaged {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(AgentEventPayload::AgentMessaged(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<AgentEventPayload>(value.clone()).ok()? {
            AgentEventPayload::AgentMessaged(messaged) => Some(messaged),
            AgentEventPayload::AgentMetrics(_) => None,
        }
    }
}

impl AgentMetricsSnapshot {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(AgentEventPayload::AgentMetrics(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<AgentEventPayload>(value.clone()).ok()? {
            AgentEventPayload::AgentMetrics(metrics) => Some(metrics),
            AgentEventPayload::AgentMessaged(_) => None,
        }
    }
}

/// Stable turn-item extension kind for a child workflow-run rollup. The
/// delegation mirror publishes one into the PARENT session whenever a
/// child session's pinned Loom workflow makes a material transition
/// (node advance, gate wait, terminal). Clients that know the kind route
/// it to the child's chip; older clients keep the generic quiet row.
pub const AGENT_GRAPH_ROLLUP_EXTENSION_KIND: &str = "agent_graph_rollup_v1";

/// Compact, render-ready state of one child's pinned workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGraphRollupV1 {
    /// The child agent this rollup describes (the chip identity).
    pub agent: AgentId,
    /// Registry workflow id when the pinned template digest joins a
    /// registered Loom workflow; `None` for ad-hoc templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// The pinned instance's template digest (the drift-proof join key).
    pub template_digest: String,
    /// "running" | "gate" | "complete" | "failed" — additive vocabulary;
    /// clients render unknown states as running.
    pub state: String,
    /// 1-based ordinal of the current node while running.
    pub node_index: u64,
    pub nodes_total: u64,
    /// Nodes whose gates are fully satisfied.
    pub nodes_green: u64,
    /// The current node's source name, when one is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_label: Option<String>,
    /// The current node's typed specialist (Loom agent-type id), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Pending gate vocabulary when `state == "gate"` (cmd/ship/human/…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
}
