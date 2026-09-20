//! Durable, presentation-neutral review data for file mutations that require
//! permission. The review rides the ordinary permission menu so the answer is
//! still a typed [`crate::menu::DecisionKind`] and the effect lifecycle stays
//! the authority for the eventual outcome.

use crate::ids::EffectId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReview {
    pub effect: EffectId,
    pub path: String,
    pub operation: FileReviewOperation,
    pub old_digest: Option<String>,
    pub new_digest: String,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<FileDiffHunk>,
    /// True when the producer bounded an unusually large diff. Counts and
    /// digests still describe the complete proposed mutation.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileReviewOperation {
    Edit,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<FileDiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffLine {
    pub kind: FileDiffLineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffLineKind {
    Context,
    Addition,
    Removal,
}
