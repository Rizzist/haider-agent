//! Tool surface contracts (§9): manifests declare effects and a dispatch mode;
//! results are bounded previews + artifact refs — full payloads live in the CAS.

use crate::effect::EffectClass;
use crate::ids::ArtifactRef;
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
}

/// Attachment blocks — composer paste tokens and skill inclusions (§10, §9.4).
/// The tree stores refs, never copies; the prompt compiler injects content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Inline `/skill` reference, pinned by hash (reserved until §9.4 lands).
    Skill {
        name: String,
        version_hash: String,
    },
}
