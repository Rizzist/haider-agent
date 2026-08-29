//! Lifecycle state machines — the product's core contract (§3).
//!
//! Freeze decisions (ADR-1):
//! - Harness status is SEPARATE from session state (`starting` belongs to the
//!   harness, not to session-open).
//! - `idle(i)` is `SessionState::Idle { interrupted: true }` plus a decay event.
//! - `input_required` carries the blocking `MenuId` — menus and input requests
//!   are ONE primitive.
//! - Run states include `verifying` and `concluding` (§9.2 symmetric gate).
//! - `waiting` always carries a mandatory typed reason — no misc bucket.
//! - Terminal states never change. Parked states never exit to idle; they exit
//!   into the work resuming.

use crate::ids::MenuId;
use serde::{Deserialize, Serialize};

/// Global daemon/process readiness — NOT a session property.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HarnessStatus {
    Starting { checks: Vec<ReadinessCheck> },
    Ready,
    ShuttingDown,
}

/// One line of the boot readiness checklist (shown on the boot screen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadinessCheck {
    pub name: String,
    pub ok: bool,
    pub duration_ms: u64,
}

/// Session lifecycle. `Idle { interrupted: true }` is the visible idle(i)
/// marker after user cancellation, drain-caused cancellation, recovery,
/// panic, or failed recovery resumption; it decays to plain idle via
/// `IdleDecayed`. Natural `Done` and ordinary provider/error completion use
/// `false`, including a natural completion that happens to win a drain race.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Restoring,
    Idle { interrupted: bool },
    ActiveRun,
    Pausing,
    Paused,
    Closing,
    Closed,
}

/// Run (turn) states. The projector reduces activity spans to ONE primary
/// state by fixed precedence so every client renders identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Thinking,
    Streaming,
    RunningTool,
    Waiting {
        reason: WaitReason,
    },
    /// A retryable provider request failed before committing any content for
    /// this turn; the actor is backing off before re-issuing it (W-C M4,
    /// Claude-Code-style visible retry). `attempt` is the NEXT attempt number
    /// (a first failure shows `attempt 2`), `max` the ceiling, `delay_ms` the
    /// backoff being waited, and `reason` distinguishes rate-limit from
    /// generic provider backoff. Non-terminal and auto-continuing: it either
    /// re-enters `Thinking` on the next try or latches `Errored` once the
    /// attempts are exhausted. Distinct from `Waiting { ProviderBackoff }`
    /// because only this variant carries the visible attempt counter.
    Retrying {
        attempt: u32,
        max: u32,
        delay_ms: u64,
        reason: WaitReason,
    },
    /// Blocked on a typed menu (question/choice/secret/file/conflict).
    InputRequired {
        menu: MenuId,
    },
    /// Blocked on a permission menu specifically (rendered distinctly).
    PermissionRequired {
        menu: MenuId,
    },
    Compacting,
    /// The gate pack is running (badge shows the sub-step).
    Verifying {
        step: VerifyStep,
    },
    /// Green received — final beat: summary/follow-through, then commit.
    Concluding,
    /// A crash interrupted a dispatched side effect — honesty state.
    EffectOutcomeUnknown,
    Cancelling,
    Done,
    Errored,
    Cancelled,
}

/// Mandatory reason on every wait. `Other` requires a non-empty tag and is
/// for extensions, never a dumping ground.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum WaitReason {
    /// A provider request is paused because the local OS positively reports
    /// no usable route. Distinct from provider backoff: the provider is not
    /// blamed, and the absolute run deadline remains armed.
    NetworkUnavailable,
    ProviderBackoff,
    RateLimit,
    RemoteChild,
    LocalChild,
    DeviceUnreachable,
    BlockingHook,
    Dependency,
    /// Parked during the brief snapshot seal (workspace write barrier).
    VerifyHold,
    /// Queued behind another gate on this workspace's verifier.
    VerifyQueue,
    /// New intersecting task deferred behind unsettled verification debt.
    WorkspaceSettlement,
    /// Detached session holding a deferred commit awaiting its covering gate.
    WorkspaceVerify,
    Other {
        tag: String,
    },
}

/// Which gate-pack member is executing (v0.1: check only; format/test reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStep {
    Check,
    Format,
    Test,
}

impl RunState {
    /// Terminal states never change (freeze rule). Live-worker enforcement is
    /// transactionally centralized in `haider-store`'s worker append gate.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunState::Done | RunState::Errored | RunState::Cancelled
        )
    }

    /// Parked states auto-continue — they may never transition directly to a
    /// session-idle projection (§3 law).
    pub fn is_parked(&self) -> bool {
        matches!(
            self,
            RunState::Waiting { .. }
                | RunState::Retrying { .. }
                | RunState::InputRequired { .. }
                | RunState::PermissionRequired { .. }
                | RunState::Compacting
                | RunState::Verifying { .. }
                | RunState::EffectOutcomeUnknown
        )
    }
}
