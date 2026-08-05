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
}

impl SessionPermissionOverridesV1 {
    /// Whether this value grants no permissions and is equivalent to absence.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.allow_writes && !self.allow_exec
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
    /// Durable creation time in Unix milliseconds.
    pub created_at_ms: u64,
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
            SessionConfigEventPayload::ModelSelected(_) => None,
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
            SessionConfigEventPayload::SessionRenamed { .. } => None,
        }
    }
}
