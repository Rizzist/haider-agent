//! Durable workspace checkpoints and the typed undo/redo contract.
//!
//! A checkpoint freezes the bytes that existed immediately before one
//! filesystem mutation. Absence is represented by `pre_artifact: None` with
//! no truncation reason; an omitted oversized or unsupported pre-image always
//! carries a reason, so capture can never fail open silently.

use crate::ids::{
    ArtifactRef, BranchId, CheckpointId, EffectId, RunId, SessionId, WorkspaceRevision,
};
use serde::{Deserialize, Serialize};

/// Maximum bytes frozen for one file pre-image (8 MiB).
pub const CHECKPOINT_PREIMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum rows returned by one checkpoint list page.
pub const CHECKPOINT_LIST_MAX_PAGE: u16 = 100;

/// User-visible mutation category. Copying to a previously absent path is a
/// create; overwriting through `fs_path` is a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Edit,
    Write,
    Create,
    Delete,
    Move,
}

impl CheckpointKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Move => "move",
        }
    }
}

/// Why the checkpoint was produced. Undo, redo, and rollback remain ordinary
/// append-only history entries and can themselves be undone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointOrigin {
    #[default]
    Tool,
    Undo,
    Redo,
    RollbackTurn,
}

impl CheckpointOrigin {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::RollbackTurn => "rollback turn",
        }
    }
}

/// Exact before/after state for one workspace-relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPath {
    pub path: String,
    /// `None` plus no truncation reason is the explicit absent marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_artifact: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_digest: Option<String>,
    /// `None` means the mutation left this path absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<String>,
}

/// Journal fact committed after the mutation outcome and before its tool
/// result is released. The store stamps `seq`, `workspace_revision`, and
/// `recorded_at_ms` from the immutable envelope in the same transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecorded {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<BranchId>,
    pub run_id: RunId,
    pub effect_id: EffectId,
    pub call_id: String,
    /// Zero is a producer placeholder and is rejected outside store stamping.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
    pub kind: CheckpointKind,
    #[serde(default)]
    pub origin: CheckpointOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_checkpoint_id: Option<CheckpointId>,
    pub paths: Vec<CheckpointPath>,
    /// Aggregate structural digest already carried by the mutation outcome.
    pub post_digest: String,
    /// Zero is a producer placeholder stamped from `committed_at_ms`.
    pub recorded_at_ms: u64,
}

/// Opaque newest-first list cursor. It is the last emitted journal sequence;
/// the next page returns rows strictly older than it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointCursor(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointListPage {
    pub checkpoints: Vec<CheckpointRecorded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<CheckpointCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointConflict {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_digest: Option<String>,
}

/// Typed all-or-nothing rollback preflight failure. `verified` names every
/// path whose freshness matched; no path was restored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRollbackConflict {
    pub verified: Vec<String>,
    pub conflicts: Vec<CheckpointConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMutationReceipt {
    pub checkpoint: CheckpointRecorded,
    pub restored_checkpoint_ids: Vec<CheckpointId>,
    pub worker_generation: u64,
}
