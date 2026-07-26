//! Agents and delegation (§5): one abstraction at every scale. A child is the
//! same object local or remote; placement is a manifest field. Callsigns are
//! display identity ONLY — the wire keys everything by opaque id.

use crate::ids::{AgentId, DeviceId, LeaseId, WorkspaceRevision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent: AgentId,
    pub role: AgentRole,
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
    Device { device: DeviceId },
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
