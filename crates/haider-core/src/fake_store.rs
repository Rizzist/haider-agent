//! In-memory [`StoreHandle`] used by tests, self-test, and the thin v0 CLI.
//!
//! Reference semantics for the B1 merge seam: contiguous per-session `seq`
//! starting at 1, one `committed_at_ms` per batch, batches never span
//! sessions. The durable store must preserve exactly these observable rules.

use crate::{CommittedRange, StoreHandle, unix_time_ms};
use async_trait::async_trait;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::SessionId;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Ephemeral store used by tests, self-test, and the thin v0 CLI.
#[derive(Debug, Default)]
pub struct MemoryStore {
    sessions: Mutex<HashMap<SessionId, Vec<RawEnvelope>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Full committed journal for one session, in `seq` order.
    pub async fn events(&self, session_id: &SessionId) -> Vec<RawEnvelope> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl StoreHandle for MemoryStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let Some(first) = envelopes.first() else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "cannot append an empty envelope batch",
                false,
            ));
        };
        let session_id = first.session_id.clone();
        if envelopes
            .iter()
            .any(|envelope| envelope.session_id != session_id)
        {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "one append batch cannot span sessions",
                false,
            ));
        }

        let mut sessions = self.sessions.lock().await;
        let journal = sessions.entry(session_id).or_default();
        let first_seq = u64::try_from(journal.len())
            .ok()
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "session sequence space exhausted",
                    false,
                )
            })?;
        let committed_at_ms = unix_time_ms();

        for (offset, envelope) in envelopes.iter_mut().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "append batch is too large to sequence",
                    false,
                )
            })?;
            envelope.seq = first_seq.checked_add(offset).ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    "session sequence space exhausted",
                    false,
                )
            })?;
            envelope.committed_at_ms = committed_at_ms;
        }

        let last_seq = envelopes
            .last()
            .map(|envelope| envelope.seq)
            .unwrap_or(first_seq);
        journal.extend(envelopes.iter().cloned());
        Ok(CommittedRange {
            first_seq,
            last_seq,
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        Ok(self
            .sessions
            .lock()
            .await
            .get(session_id)
            .into_iter()
            .flatten()
            .filter(|envelope| envelope.seq > since_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        Ok(self
            .sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|journal| journal.last())
            .map_or(0, |envelope| envelope.seq))
    }
}
