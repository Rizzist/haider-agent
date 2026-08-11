//! Tool surface contracts (§9): manifests declare effects and a dispatch mode;
//! results are bounded previews + artifact refs — full payloads live in the CAS.

use crate::effect::EffectClass;
use crate::ids::ArtifactRef;
use crate::item::ToolStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    /// Normalized effects this tool can produce — the permission key.
    pub effects: Vec<EffectClass>,
    pub dispatch: DispatchMode,
    /// JSON Schema for the input (kept opaque here).
    pub input_schema: serde_json::Value,
}

/// Dispatch is a lifecycle decision, not a handler detail (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchMode {
    Await,
    FireAndForget,
    /// Returns a correlation ticket now; a later tool_result wakes the turn.
    Deferred,
}

/// Daemon policy applied when no durable session grant narrows the decision.
///
/// This is inventory data, not an approval credential. Clients may display it
/// but must never use it to authorize an effect or synthesize a menu answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionDefault {
    /// The tool has no brokered effect (for example `request_input`).
    NotApplicable,
    Allow,
    Ask,
    Deny,
}

/// One canonical registry entry projected for read-only clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInventoryEntry {
    pub manifest: ToolManifest,
    pub default: ToolPermissionDefault,
}

/// Durable scope of a remembered permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum RememberedGrantScope {
    /// All operations in the normalized effect class for this session.
    Class,
    /// Only one canonical process shape. The digest binds exact command
    /// bytes, canonical cwd, and sorted environment-name allowlist.
    CommandShape { args_digest: String },
}

/// Sanitized projection of one grant reconstructed from durable menu facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedSessionGrant {
    pub class: EffectClass,
    pub scope: RememberedGrantScope,
}

/// Read-only daemon inventory for a session.
///
/// The registered tools come from the same manifests used for provider
/// advertisement. Remembered grants are reconstructed from that session's
/// effect/menu journal; this snapshot cannot answer or create a menu.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInventorySnapshot {
    #[serde(default)]
    pub tools: Vec<ToolInventoryEntry>,
    #[serde(default)]
    pub remembered_grants: Vec<RememberedSessionGrant>,
}

/// Every tool result is bounded: a preview the prompt can afford, plus refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundedResult {
    pub preview: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    /// Opaque continuation for paging the full result on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Honest terminal disposition. Completed is omitted to preserve the
    /// existing success wire shape; all failure values are additive.
    #[serde(default, skip_serializing_if = "ToolResultStatus::is_completed")]
    pub status: ToolResultStatus,
    /// Bounded, sanitized human-readable reason for a non-success disposition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Terminal result status carried independently of the tool-call lifecycle.
/// New values append only: durable encodings use names, never ordinals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    #[default]
    Completed,
    Rejected,
    Conflict,
    Failed,
    Cancelled,
    Unknown,
}

impl ToolResultStatus {
    #[must_use]
    pub const fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    #[must_use]
    pub const fn item_status(self) -> ToolStatus {
        match self {
            Self::Completed => ToolStatus::Completed,
            Self::Rejected => ToolStatus::Rejected,
            Self::Conflict => ToolStatus::Conflict,
            Self::Failed => ToolStatus::Failed,
            Self::Cancelled => ToolStatus::Cancelled,
            Self::Unknown => ToolStatus::Unknown,
        }
    }
}

/// Attachment blocks — composer paste tokens and skill inclusions (§10, §9.4).
/// The tree stores refs, never copies; the prompt compiler injects content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentBlock {
    Image {
        artifact: ArtifactRef,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
    PastedText {
        artifact: ArtifactRef,
        lines: u32,
    },
    /// A UTF-8 text FILE attached by path (G2). Like [`Self::PastedText`]
    /// the tree stores the CAS ref, never the bytes; the prompt compiler
    /// inlines the content with a `<file name=… lines=…>` header so the
    /// model sees the filename. `name` is the sanitized BASENAME only
    /// (never a full path — privacy), ≤ 120 chars, no control characters.
    File {
        artifact: ArtifactRef,
        name: String,
        lines: u32,
    },
    /// Inline `/skill` reference, pinned by hash (reserved until §9.4 lands).
    Skill {
        name: String,
        version_hash: String,
    },
}
