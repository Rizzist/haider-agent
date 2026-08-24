//! Durable control-plane facts for user messages held behind an active turn.

use crate::DeliveryMode;
use crate::ids::EventId;
use serde::{Deserialize, Serialize};

/// One render-complete held-message row.
///
/// `id` is the committed user-message event id, so it is stable across list
/// calls, daemon restarts, and mutations of other rows. `ordinal` is one-based
/// and describes this snapshot only; clients mutate by `id`, never ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRow {
    pub id: EventId,
    pub text: String,
    pub mode: DeliveryMode,
    pub ordinal: u32,
    pub created_at_ms: u64,
}

/// The typed change carried by a [`QueueDelta`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QueueChange {
    /// A newly held message. The complete row prevents rendering drift across
    /// clients; consumers never reconstruct display text from another event.
    Enqueued {
        row: QueueRow,
    },
    Removed {
        id: EventId,
    },
    PromotedSteer {
        id: EventId,
    },
    Consumed {
        id: EventId,
    },
    #[serde(other)]
    Unknown,
}

/// One revision-bearing queue change on the ordinary session event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueDelta {
    pub revision: u64,
    pub change: QueueChange,
}
