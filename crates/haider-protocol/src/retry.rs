//! Additive durable facts for manual run retry.
//!
//! This union stays separate from [`crate::EventPayload`] so existing
//! exhaustive Rust consumers remain source-compatible. Raw-envelope readers
//! preserve the fact, while retry-aware reducers can decode its source
//! coordinates for prompt compilation and restart recovery.

use crate::ids::RunId;
use serde::{Deserialize, Serialize};

/// Additive journal facts owned by the `run.retry` command family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunRetryEventPayload {
    /// A fresh run was manually started from an already-committed failed user
    /// turn. The source message remains the single durable `UserMessage`.
    RunRetried {
        failed_run_id: RunId,
        prompt_run_id: RunId,
        user_seq: u64,
    },
}

impl RunRetryEventPayload {
    /// Encodes one retry fact for a raw durable envelope.
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }

    /// Decodes one retry fact from an additive raw payload.
    pub fn from_payload_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}
