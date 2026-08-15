//! Long-lived background shell tasks (W-A).
//!
//! Additive task-event union kept separate from [`crate::EventPayload`] so
//! existing exhaustive consumers stay source-compatible. Raw-envelope readers
//! preserve these newer event kinds; the journal is the truth and every
//! in-memory task registry is a projection of these facts.

use crate::effect::WorkspaceMutation;
use crate::ids::{ArtifactRef, TaskId};
use serde::{Deserialize, Serialize};

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Rolling tail preview retained per task for cheap reads (bytes).
pub const TASK_TAIL_BYTES: usize = 4 * 1024;

/// Hard cap on retained task output (bytes). Output beyond the cap is
/// dropped and the truncation is marked honestly — the task keeps running.
pub const TASK_OUTPUT_RETAIN_BYTES: usize = 512 * 1024;

/// Hard cap on concurrent background tasks per session.
pub const TASK_CONCURRENCY_CAP: usize = 8;

/// Terminal disposition of one background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskTerminalState {
    /// The leader exited on its own; `exit_code` is `None` when it was ended
    /// by a signal.
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// The supervisor lost the task without a clean exit (spawn-side fault,
    /// daemon restart orphaning, supervision error).
    Failed { reason: String },
    /// Deliberate termination (`task_kill`, session close, daemon shutdown).
    Killed,
}

/// How one task completion reached the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCompletionDelivery {
    /// A run was active: the notice was steer-injected mid-turn (the durable
    /// steer user message carries the prompt copy).
    DeliveredSteer,
    /// The session was idle: the completion fact itself carries the bounded
    /// prompt notice for the next turn.
    DeliveredQueued,
}

/// Durable start-of-life fact for one background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStarted {
    pub task: TaskId,
    /// Short display label (defaulted from the command's first token).
    pub name: String,
    /// Bounded command summary — display only, never re-executed.
    pub command: String,
    /// Process-group leader pid (`pgid == pid`; kill is a pgid kill).
    pub pid: i32,
    pub started_at_ms: u64,
}

/// Durable end-of-life fact for one background task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCompleted {
    pub task: TaskId,
    pub name: String,
    pub state: TaskTerminalState,
    pub elapsed_ms: u64,
    /// Total bytes the task produced (may exceed what was retained).
    pub output_bytes: u64,
    /// Bounded output tail preview (last [`TASK_TAIL_BYTES`], lossy UTF-8).
    pub tail: String,
    /// Full bounded output (cap [`TASK_OUTPUT_RETAIN_BYTES`]) in the CAS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    /// The bounded tail is still present, but the retained full-output CAS
    /// object could not be stored.
    #[serde(default, skip_serializing_if = "is_false")]
    pub full_output_unavailable: bool,
    /// True when `output_bytes` exceeded the retained cap.
    pub truncated: bool,
    pub delivery: TaskCompletionDelivery,
    /// Post-completion workspace provenance for a detached process that
    /// changed the tree. The store stamps its revision at this fact's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation: Option<WorkspaceMutation>,
}

/// Additive task-event union (same pattern as
/// [`crate::agent::AgentEventPayload`]): serialized into the shared tagged
/// payload namespace, decoded ad hoc from raw envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEventPayload {
    TaskStarted(TaskStarted),
    TaskCompleted(TaskCompleted),
}

impl TaskEventPayload {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

impl TaskStarted {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        TaskEventPayload::TaskStarted(self.clone()).to_payload_value()
    }
}

impl TaskCompleted {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        TaskEventPayload::TaskCompleted(self.clone()).to_payload_value()
    }
}
