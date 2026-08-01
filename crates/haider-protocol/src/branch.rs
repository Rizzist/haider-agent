//! Durable named branch references over the immutable session history tree.

use crate::ids::{BranchId, NodeId};
use serde::{Deserialize, Serialize};

/// Durable topology and head coordinates for one named branch.
///
/// The legacy/main branch is intentionally represented by `None` elsewhere
/// and never receives a concrete [`BranchId`] or registry row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDescriptor {
    pub branch_id: BranchId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch_id: Option<BranchId>,
    pub fork_node_id: NodeId,
    pub fork_seq: u64,
    pub created_seq: u64,
    pub created_at_ms: u64,
    pub head_node_id: NodeId,
    pub head_seq: u64,
}

/// Additive replay fact emitted atomically with a new branch registry row and
/// its command receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCreated {
    pub branch: BranchDescriptor,
}

/// Additive topology-event union kept separate from [`crate::EventPayload`]
/// so existing exhaustive Rust consumers remain source compatible during
/// B2a. Readers should try this decoder before treating an unknown core event
/// kind as opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BranchEventPayload {
    BranchCreated(BranchCreated),
}

impl BranchCreated {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(BranchEventPayload::BranchCreated(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<BranchEventPayload>(value.clone()).ok()? {
            BranchEventPayload::BranchCreated(created) => Some(created),
        }
    }
}
