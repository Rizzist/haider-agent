//! Hook configuration vocabulary, additive journal facts, and startup gates.
//!
//! Hook runtime facts intentionally do not enter the closed [`crate::EventPayload`]
//! enum. They use their own tagged union and ride [`crate::envelope::RawEnvelope`]
//! as additive payload kinds, so every pre-H2 typed reducer continues to
//! tolerate them as unknown facts.

use crate::DeliveryMode;
use crate::ids::{ArtifactRef, BranchId, RunId, SessionId};
use serde::{Deserialize, Serialize};

pub const HOOKS_CONFIG_SCHEMA: &str = "haider.hooks.v1";
pub const HOOKS_LIST_SCHEMA: &str = "haider.observe.v1";

/// Sanitized stdin/JSONL projection for hook facts that cannot safely expose
/// their journal payload verbatim. Ordinary hook events remain raw envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HookInput {
    /// One canonical committed user-message acceptance, independent of the
    /// TUI, RPC, headless, or voice surface that submitted it.
    UserMessage {
        session: SessionId,
        run: RunId,
        branch: Option<BranchId>,
        mode: DeliveryMode,
        text: String,
        truncated: bool,
        attachments: HookAttachmentSet,
    },
}

/// Metadata-only attachment inventory for one hook input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookAttachmentSet {
    pub count: u32,
    pub items: Vec<HookAttachmentMetadata>,
}

/// Safe attachment coordinates. `bytes` is a length; resolved artifact bytes
/// are never part of hook input JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookAttachmentMetadata {
    pub mime: String,
    pub bytes: u64,
    /// The immutable BLAKE3 artifact digest, never artifact contents.
    pub artifact: ArtifactRef,
}

/// Additive hook-engine facts written to the session journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEventPayload {
    HookNotice(HookNotice),
    HookFired(HookFired),
    HookSubscription(HookSubscription),
    /// Profile/global producers may project update truth into a session.
    UpdateAvailable {
        version: String,
    },
    /// Profile/global producers may project account expiry into a session.
    AccountExpired {
        provider: String,
        alias: String,
    },
    /// Run-scoped, durable authority created only by `haider run
    /// --trust-hooks`; it never changes the profile trust set.
    HookRunTrust {
        enabled: bool,
    },
}

impl HookEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    pub fn from_payload_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub fn is_engine_fact(value: &serde_json::Value) -> bool {
        matches!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("hook_notice" | "hook_fired" | "hook_subscription")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRuntimeKind {
    Exec,
    Subscribe,
    Decision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookFired {
    pub hook: String,
    pub digest: String,
    pub kind: HookRuntimeKind,
    pub observed_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: HookOutput,
    pub stderr: HookOutput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_decision: Option<HookDecisionKind>,
    /// `true` only when the menu CAS committed this hook's answer. A proposal
    /// that lost to another surface is never represented as authority.
    pub decision_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOutput {
    pub preview: String,
    pub bytes: u64,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecisionKind {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSubscription {
    pub hook: String,
    pub digest: String,
    pub state: HookSubscriptionState,
    pub restart_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSubscriptionState {
    Started,
    Exited,
    RestartScheduled,
    Stopped,
}

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
