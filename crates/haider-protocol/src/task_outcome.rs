//! Explicit model-reported task outcomes, carried by the existing tool stream.
//!
//! V1 adds a failure signal. Ordinary provider completion remains the success
//! path, including its finalization guards; assistant text is never a signal.

use serde::{Deserialize, Serialize};

/// Additive `task_outcome` field on the durable terminal run-state envelope.
/// This is a model report, not independent verification of the task's effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskOutcomeV1 {
    Failure { reason: String },
}

impl TaskOutcomeV1 {
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Failure { reason } => reason,
        }
    }
}
