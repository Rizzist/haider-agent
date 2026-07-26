//! Session runtime for the Haider harness.
//!
//! [`actor::HarnessActor`] drives one session's turns: provider stream events
//! become committed protocol envelopes. This crate also owns the runtime side
//! of the W1 store seam: [`StoreHandle`] mirrors haider-store's `EventStore`
//! surface so the B1 (store) and B2 (runtime) patches merge cleanly without
//! this crate depending on haider-store yet.
//!
//! Seam contract (`StoreHandle`, kept in lockstep with B1):
//! - `append` assigns `seq` and `committed_at_ms` in place; `seq` is
//!   contiguous per session starting at 1, and one batch never spans sessions.
//! - The runtime may publish an envelope only after `append` has returned
//!   success — durable-before-visible.
//! - `event_id` uniqueness is minted by the runtime (see
//!   `HarnessActor::next_event_id`), not the store.

mod actor;
mod fake_store;

pub use actor::{
    CancelToken, HarnessActor, HarnessConfig, HarnessHandle, SubmitTurn, TurnHandle, TurnOutcome,
};
pub use fake_store::MemoryStore;

use async_trait::async_trait;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::SessionId;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-core";

/// Sequence allocation returned by an atomic store append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedRange {
    pub first_seq: u64,
    pub last_seq: u64,
}

/// Durability port consumed by the runtime.
///
/// The store assigns `seq` and `committed_at_ms` in place. Only after this
/// method returns successfully may the runtime publish the envelopes.
#[async_trait]
pub trait StoreHandle: Send + Sync {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError>;

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError>;

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError>;
}

/// Wall-clock milliseconds since the Unix epoch, saturating at the extremes.
pub(crate) fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
