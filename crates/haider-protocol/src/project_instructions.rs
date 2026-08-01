//! Durable audit facts for daemon-loaded project instructions.

use serde::{Deserialize, Serialize};

/// One file body that contributed to a pinned system prompt.
///
/// The digest and byte count cover the exact effective UTF-8 body inserted
/// into the prompt, including the truncation marker when present. The body
/// itself is deliberately not copied into the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstructionFileFact {
    pub path: String,
    pub digest: String,
    pub bytes: u64,
    pub truncated: bool,
}

/// Ordered instruction-file facts for one pinned logical-turn snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstructionsLoaded {
    pub files: Vec<ProjectInstructionFileFact>,
}

/// Additive project-instruction event union kept separate from
/// [`crate::EventPayload`] so existing exhaustive Rust consumers remain
/// source compatible. Raw-envelope readers preserve this newer event kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectInstructionsEventPayload {
    ProjectInstructionsLoaded(ProjectInstructionsLoaded),
}

impl ProjectInstructionsLoaded {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(ProjectInstructionsEventPayload::ProjectInstructionsLoaded(
            self.clone(),
        ))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<ProjectInstructionsEventPayload>(value.clone()).ok()? {
            ProjectInstructionsEventPayload::ProjectInstructionsLoaded(loaded) => Some(loaded),
        }
    }
}
