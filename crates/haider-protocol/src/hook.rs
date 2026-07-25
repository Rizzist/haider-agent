//! Hook decisions and startup gates (§8) — frozen now so E4 (W4) is not a
//! schema-affecting patch (review finding: lane-closing changes are the
//! expensive kind).

use serde::{Deserialize, Serialize};

/// A policy gate's decision on one operation. Durable — hooks are not re-run
/// during replay (§8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDecision {
    /// Hook identifier (config name).
    pub hook: String,
    /// Which gate point fired ("tool.authorize", "turn.before_commit", …).
    pub point: String,
    pub verdict: HookVerdict,
    /// Hash of the operation the verdict binds to — a mutated operation gets
    /// a new hash, invalidating prior approvals (§8).
    pub op_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum HookVerdict {
    Allow,
    Ask,
    Deny {
        reason: String,
    },
    NarrowCapability {
        to: Vec<String>,
    },
    /// Operation was rewritten; new op_hash supplied.
    Mutate {
        new_op_hash: String,
    },
}

/// A persisted harness-scoped startup-gate decision (trust/update screens).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartupGateDecision {
    pub kind: StartupGateKind,
    /// The chosen option key from the gate's menu.
    pub choice: String,
    pub decided_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupGateKind {
    TrustHook,
    Update,
}
