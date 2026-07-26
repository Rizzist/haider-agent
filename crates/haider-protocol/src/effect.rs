//! Effects — permissions key off normalized effects (what an operation
//! touches), never tool names (§9). The effect record is a four-phase life:
//! intent → authorized → dispatched → outcome. A crash between dispatch and
//! outcome yields `Unknown`, surfacing `effect_outcome_unknown` (§3 honesty).

use crate::ids::{EffectId, MenuId, WorkspaceRevision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum EffectClass {
    FsRead,
    FsWrite,
    ProcessExec,
    Network {
        host: String,
    },
    GitOp,
    AgentSpawn,
    CredentialAccess,
    /// Reserved (§9.3) — the computer-use pack lands post-0.1.
    GuiAct,
}

/// Journaled BEFORE dispatch — the durable pre-image of every side effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectIntent {
    pub effect: EffectId,
    pub class: EffectClass,
    /// One-line human summary ("patch src/lib.rs", "run cargo test").
    pub summary: String,
    /// Digest of normalized arguments — approvals bind to this; a mutated
    /// operation gets a new digest, invalidating prior approvals.
    pub args_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AuthorizationVerdict {
    Allow,
    /// The user directly initiated this effect (for example with `!cmd`).
    /// It is durably distinguished from a model effect allowed by policy.
    PreAuthorized {
        source: AuthorizationSource,
    },
    /// Blocked on a permission menu.
    Ask {
        menu: MenuId,
    },
    Deny {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationSource {
    UserTyped,
}

/// Effect lifecycle events as journaled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum EffectPhase {
    Intent(EffectIntent),
    Authorized {
        effect: EffectId,
        verdict: AuthorizationVerdict,
    },
    Dispatched {
        effect: EffectId,
    },
    Outcome {
        effect: EffectId,
        outcome: EffectOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EffectOutcome {
    Ok,
    /// The effect crossed the dispatch boundary and was deliberately stopped.
    /// Cancellation is a terminal outcome, never a failure.
    Cancelled,
    /// Cancellation won, but userspace could not prove process-group death.
    /// The note is durable and orderly broker close separately reports the
    /// leaked registry entry.
    CancelledEscalated {
        note: String,
    },
    Failed {
        error: String,
    },
    /// Crash window between dispatch and result — reconciled via Recovery menu.
    Unknown,
}
