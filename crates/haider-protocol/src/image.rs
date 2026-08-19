//! Durable facts describing image files created by tool calls.

use serde::{Deserialize, Serialize};

/// Stable turn-item extension kind for an image created in the workspace.
pub const IMAGE_CREATED_EXTENSION_KIND: &str = "image_created_v1";

/// Self-contained payload for an image file created by a tool call.
///
/// `path` is the canonical absolute location used by native clients, while
/// `display_path` is suitable for showing directly in a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageCreatedV1 {
    pub path: String,
    pub display_path: String,
    pub media_type: String,
    pub byte_len: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub call_id: String,
    pub tool: String,
}
