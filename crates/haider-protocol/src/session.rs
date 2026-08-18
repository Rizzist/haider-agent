//! Durable, typed session configuration.

use serde::{Deserialize, Serialize};

/// Optional, durable permissions granted when a session is created by a
/// non-interactive client.
///
/// These are ordinary policy overrides, not evidence that a human typed or
/// approved a particular effect. The daemon therefore applies them as
/// `AuthorizationVerdict::Allow`, never as `PreAuthorized(UserTyped)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionOverridesV1 {
    /// Allow model-initiated filesystem writes and patches without a menu.
    #[serde(default)]
    pub allow_writes: bool,
    /// Allow model-initiated process execution without a menu.
    #[serde(default)]
    pub allow_exec: bool,
    /// Auto-allow mode (the Codex `--full-auto` analogue): resolve EVERY
    /// effect class the model can reach to `Allow` for the session, not just
    /// writes/exec — computer control, web fetch, task-kill, and any future
    /// class included. It is a policy default flip, never a `PreAuthorized`
    /// credential and never a suppression of the deny path: an explicit deny
    /// rule still wins (the broker checks the denylist first), every effect is
    /// still journaled, the macOS TCC gate still applies to computer actions
    /// (auto-allow only lifts Haider's own menu — the OS grant flow is
    /// unchanged), and the "controlling your screen" banner still shows.
    ///
    /// Omitted from the wire while `false` so a pre-auto-allow overrides value
    /// keeps its exact historical bytes; the field only appears when enabled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_allow: bool,
}

impl SessionPermissionOverridesV1 {
    /// Whether this value grants no permissions and is equivalent to absence.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.allow_writes && !self.allow_exec && !self.auto_allow
    }
}

/// Authoritative metadata stored in `sessions.meta_json` for live sessions.
///
/// The version suffix is intentional: old rows contain `{}` and decode as no
/// typed metadata, while a future incompatible shape can be added without
/// silently reinterpreting committed configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadataV1 {
    /// Canonical absolute UTF-8 workspace path.
    pub cwd: String,
    /// Provider adapter name (`anthropic`, `openai`, `openai-compatible`, or
    /// `fake` in injected tests).
    ///
    /// Sessions are provider-agnostic: this field is plumbing for the CURRENT
    /// model selection, never session identity. The user selects a model; the
    /// provider rides along as an attribute of the selected row and both may
    /// change together through `session.select_model`.
    pub provider: String,
    /// Full provider model identifier — the current model selection.
    pub model: String,
    /// Maximum output tokens for each provider request.
    pub max_tokens: u64,
    /// Version of the deterministic daemon-owned system policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_version: Option<String>,
    /// Optional headless creation policy. Absence preserves legacy session and
    /// receipt bytes and means the daemon registry defaults remain authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_overrides: Option<SessionPermissionOverridesV1>,
    /// Optional user-facing session title (G2). `None` for legacy rows and
    /// untitled sessions — absence stays OFF the wire so pre-G2 metadata
    /// bytes are unchanged. Normalized by the daemon: trimmed, control
    /// characters stripped, ≤ 80 chars, empty collapses to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Explicit per-pair reasoning-effort selection (G3). `None` means "the
    /// provider's own default" and is skipped on the wire so pre-G3 metadata
    /// rows stay byte-identical. The value is a provider-vocabulary STRING
    /// validated against the CURRENT pair's declared ladder at selection
    /// time — never a global enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Anthropic fast-mode flag (G3). Skipped while false so pre-G3 metadata
    /// rows stay byte-identical.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fast: bool,
    /// Cache-destruction warning policy (CM3). Absent legacy metadata uses
    /// the balanced default and its configurable cold-cost threshold.
    #[serde(
        default,
        skip_serializing_if = "crate::cache::CachePolicySettingsV1::is_default"
    )]
    pub cache_policy: crate::cache::CachePolicySettingsV1,
    /// Durable creation time in Unix milliseconds.
    pub created_at_ms: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Additive replay fact emitted atomically with a committed live-session
/// model selection and its command receipt.
///
/// Sessions are provider-agnostic: the fact records the RESOLVED pair the
/// next logical turn resolves through — the model the user selected plus the
/// provider attribute of that row — not a change of session identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelected {
    /// Provider attribute of the selected model row.
    pub provider: String,
    /// The selected full model identifier.
    pub model: String,
}

/// Additive replay fact emitted atomically with a committed live-session
/// effort selection and its command receipt (G3).
///
/// `None` reverts the session to the provider default. The fact is a pure
/// session-config journal movement: it never moves the conversation tree,
/// which is what lets the F3 compaction head CAS tolerate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortSelected {
    /// The selected effort, or `None` for "provider default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Additive replay fact emitted atomically with a committed live-session
/// fast-mode toggle and its command receipt (G3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastModeSelected {
    /// Whether fast mode is on after this fact.
    pub enabled: bool,
}

/// Additive session-configuration event union kept separate from
/// [`crate::EventPayload`] so existing exhaustive Rust consumers remain
/// source compatible. Readers should try this decoder before treating an
/// unknown core event kind as opaque (the same contract as
/// [`crate::branch::BranchEventPayload`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigEventPayload {
    ModelSelected(ModelSelected),
    /// Additive replay fact emitted atomically with a committed session
    /// rename and its command receipt (G2). `None` = the title was cleared;
    /// absence stays OFF the wire.
    SessionRenamed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    EffortSelected(EffortSelected),
    FastModeSelected(FastModeSelected),
}

impl SessionConfigEventPayload {
    /// Encodes one committed rename fact.
    pub fn session_renamed_value(
        title: Option<String>,
    ) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(Self::SessionRenamed { title })
    }

    /// Decodes a `session_renamed` fact: `Some(title)` when the payload is
    /// one, `None` for every other payload.
    #[must_use]
    pub fn session_renamed_from_value(value: &serde_json::Value) -> Option<Option<String>> {
        match serde_json::from_value::<Self>(value.clone()).ok()? {
            Self::SessionRenamed { title } => Some(title),
            _ => None,
        }
    }
}

impl ModelSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::ModelSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::ModelSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl EffortSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::EffortSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::EffortSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl FastModeSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::FastModeSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::FastModeSelected(selected) => Some(selected),
            _ => None,
        }
    }
}
