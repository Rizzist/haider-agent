//! Tool surface contracts (§9): manifests declare effects and a dispatch mode;
//! results are bounded previews + artifact refs — full payloads live in the CAS.

use crate::effect::EffectClass;
use crate::error::ErrorPresentation;
use crate::ids::ArtifactRef;
use crate::item::ToolStatus;
use serde::{Deserialize, Serialize};

/// Maximum encoded size of one image produced by a tool.
///
/// This deliberately matches the existing composer attachment ceiling: one
/// image has one storage and provider-ingress law regardless of its origin.
pub const TOOL_RESULT_IMAGE_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Maximum encoded bytes accepted from a native screenshot before any image
/// decoder runs. Redaction and CAS admission share this preflight ceiling.
pub const TOOL_RESULT_IMAGE_MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
/// Maximum decoded source pixels accepted before redaction/downscaling.
pub const TOOL_RESULT_IMAGE_MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// Maximum allocation permitted to a PNG decoder at either boundary.
pub const TOOL_RESULT_IMAGE_MAX_DECODE_ALLOC: u64 = 192 * 1024 * 1024;
/// Maximum width or height retained for a tool-produced image. Larger source
/// screenshots are downscaled before they enter the CAS.
pub const TOOL_RESULT_IMAGE_MAX_DIMENSION: u32 = 2_048;
/// Maximum tool-result images admitted to one provider request.
pub const TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN: usize = 5;
/// Maximum encoded tool-result image bytes admitted to one provider request.
/// This matches the existing composer turn aggregate ceiling.
pub const TOOL_RESULT_IMAGE_MAX_BYTES_PER_TURN: u64 = 16 * 1024 * 1024;

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
    /// Bounded image artifacts produced by the tool. The journal stores only
    /// these metadata refs; provider-bound bytes are resolved from the CAS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageBlockRef>,
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
    /// Safe structured presentation for every non-success result. Optional
    /// only for backward-compatible decoding of pre-E2 journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ErrorPresentation>,
}

/// One validated, bounded image in the artifact store.
///
/// All fields describe the exact encoded CAS object. Image bytes never ride
/// this protocol value or any journal envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlockRef {
    pub artifact: ArtifactRef,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: u64,
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
    /// A first-class PDF. Bytes stay in the daemon CAS; `pages` is the
    /// daemon-verified page-tree count and `delivery` is the provider shaping
    /// mode durably selected when the turn was accepted.
    Pdf {
        artifact: ArtifactRef,
        name: String,
        pages: u32,
        #[serde(default)]
        delivery: PdfDeliveryMode,
    },
    /// Inline `/skill` reference, pinned by hash (reserved until §9.4 lands).
    Skill {
        name: String,
        version_hash: String,
    },
}

/// Durable provider shaping used for a PDF attachment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfDeliveryMode {
    NativeDocument,
    #[default]
    ExtractedText,
}

/// Receipt projection for PDFs in an accepted turn. The command receipt and
/// the journaled [`AttachmentBlock::Pdf`] agree on these facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfAttachmentReceipt {
    pub artifact: ArtifactRef,
    pub pages: u32,
    pub delivery: PdfDeliveryMode,
}
