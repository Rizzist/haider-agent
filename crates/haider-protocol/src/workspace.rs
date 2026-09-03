//! Additive workspace-availability and workspace-selection journal facts.

use serde::{Deserialize, Serialize};

/// Why a session's stored workspace root cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceUnavailableReason {
    Missing,
    NotDirectory,
    NotReadable,
}

impl WorkspaceUnavailableReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotDirectory => "not_directory",
            Self::NotReadable => "not_readable",
        }
    }
}

/// A cheap availability check found that the stored workspace cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceUnavailable {
    /// The stored absolute root. It is reported verbatim so recovery can name
    /// the session metadata that needs replacing.
    pub path: String,
    pub reason: WorkspaceUnavailableReason,
    /// A bounded, display-safe explanation of the failed check.
    pub detail: String,
}

/// A receipt-backed session workspace mutation committed a new canonical root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSelected {
    pub path: String,
    /// Root whose live hook subscribers/servers must be retired. Absent only
    /// on legacy facts written before this additive field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
}

/// Additive workspace event union kept separate from [`crate::EventPayload`]
/// so existing exhaustive Rust consumers remain source compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEventPayload {
    WorkspaceUnavailable(WorkspaceUnavailable),
    WorkspaceSelected(WorkspaceSelected),
}

impl WorkspaceEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}
