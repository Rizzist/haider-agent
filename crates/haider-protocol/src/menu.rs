//! The Menu primitive — every interactive card is one typed, answerable object
//! (§3). Menus render identically in TUI/GUI/headless and are answerable by id
//! from any surface; `input.resolved { via }` reconciles the others.
//!
//! Freeze decision (ADR-1): `input_required` is unified with Menu — a run
//! blocked on input carries a `MenuId`; there is no separate input-request type.

use crate::ids::{AgentId, MenuId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Menu {
    pub id: MenuId,
    pub kind: MenuKind,
    pub title: String,
    #[serde(default)]
    pub body: Vec<String>,
    pub options: Vec<MenuOption>,
    /// Whether the owning run is blocked until answered.
    pub blocking: bool,
    pub scope: MenuScope,
    /// Free-form origin tag (tool name, subsystem) for display/audit.
    pub origin: String,
    /// Milliseconds-since-epoch expiry; expired menus resolve via timeout policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    /// Option index selected when ttl expires (defaults to none = turn errored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_option: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MenuKind {
    /// Tool-call approval: effect class + the exact rule an "always" creates.
    Permission {
        effect_summary: String,
    },
    /// effect_outcome_unknown reconciliation (probe / retry / mark errored).
    /// Reconciliation re-emits the effect's terminal `Outcome` phase.
    Recovery {
        effect: crate::ids::EffectId,
    },
    /// All provider accounts rate-limited (wait-with-auto-resume / stop).
    Exhausted,
    TrustHook,
    Update,
    Question,
    Choice,
    /// Free-form secret entry. The ANSWER never carries the secret: the client
    /// resolves it via a dedicated non-journaled vault RPC and answers with a
    /// vault reference (secrets-never-in-protocol law).
    Secret,
    /// File/path selection.
    File,
    /// Workspace conflict (e.g. editing a component under active repair).
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MenuOption {
    /// Stable key — answers reference the key OR the index.
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Decision semantics (ACP-mappable). The server ENUMERATES the options;
    /// clients render, never invent (codex retrofit lesson).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    AllowOnce,
    /// Creates a persistent rule — the menu body states EXACTLY which rule.
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// Placement follows scope (§3): session-scoped replaces the composer;
/// subagent-scoped replaces the subagent view's composer; harness-scoped
/// renders as a full-screen startup gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum MenuScope {
    Session,
    Subagent { agent: AgentId },
    Harness,
}

/// An answer, from any surface. `via` records which surface answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MenuAnswer {
    pub menu: MenuId,
    /// Preferred: answer by stable option key (immune to re-issued menus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_key: Option<String>,
    /// Fallback: answer by index (racy across menu re-issue; key wins if both).
    pub option_index: u32,
    /// Free-form value for Question/Secret/File kinds. For Secret this is a
    /// VAULT REFERENCE, never the raw secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub via: AnswerVia,
}

/// Why an unanswered menu ceased to be answerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuCloseReason {
    Cancelled,
    Dismissed,
    /// Daemon recovery could not safely resume the interrupted run.
    RecoveryInterrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerVia {
    Tui,
    Gui,
    Rpc,
    Voice,
    Timeout,
}
