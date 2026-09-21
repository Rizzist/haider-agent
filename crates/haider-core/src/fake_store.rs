//! In-memory [`StoreHandle`] used by tests and the offline self-test.
//!
//! Reference semantics for the B1 merge seam: contiguous per-session `seq`
//! starting at 1, one `committed_at_ms` per batch, batches never span
//! sessions. The durable store must preserve exactly these observable rules.

use crate::{
    CommittedRange, ReducerPage, ReducerPageCursor, SessionProjectionCheckpoint, StoreHandle,
    envelope_payload_kind, unix_time_ms,
};
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{BranchId, SessionId};
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

/// Ephemeral store used by tests and the offline self-test.
#[derive(Debug, Default)]
pub struct MemoryStore {
    sessions: Mutex<HashMap<SessionId, Vec<RawEnvelope>>>,
    checkpoints: Mutex<HashMap<(SessionId, String, String), SessionProjectionCheckpoint>>,
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

    async fn read_reducer_page_with_boundary(
        &self,
        session_id: &SessionId,
        cursor: ReducerPageCursor,
        limit: usize,
        byte_budget: usize,
        payload_kinds: &'static [&'static str],
    ) -> Result<ReducerPage, HaiderError> {
        let sessions = self.sessions.lock().await;
        let journal = sessions
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let observed_head = journal
            .last()
            .map(|envelope| (envelope.seq, envelope.event_id.clone()));
        let observed_fence = cursor.fence_seq.and_then(|fence_seq| {
            journal
                .iter()
                .find(|envelope| envelope.seq == fence_seq)
                .map(|envelope| (envelope.seq, envelope.event_id.clone()))
        });
        let mut envelopes = Vec::new();
        let mut spent = 0_usize;
        if limit > 0 && !payload_kinds.is_empty() {
            for envelope in journal.iter().filter(|envelope| {
                envelope.seq > cursor.after_seq
                    && payload_kinds.contains(&envelope_payload_kind(envelope))
            }) {
                let weight = haider_protocol::envelope::envelope_weight_bytes(envelope);
                if !envelopes.is_empty() && spent.saturating_add(weight) > byte_budget {
                    break;
                }
                spent = spent.saturating_add(weight);
                envelopes.push(envelope.clone());
                if envelopes.len() >= limit || spent >= byte_budget {
                    break;
                }
            }
        }
        Ok(ReducerPage {
            envelopes,
            observed_head,
            observed_fence,
            // This store exposes no rewrite/delete surface; its journal is
            // append-only for the lifetime of the in-memory session.
            observed_mutation_generation: Some(0),
        })
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

    async fn projection_checkpoint(
        &self,
        session_id: &SessionId,
        projection: &str,
        timeline_key: &str,
    ) -> Result<Option<SessionProjectionCheckpoint>, HaiderError> {
        Ok(self
            .checkpoints
            .lock()
            .await
            .get(&(
                session_id.clone(),
                projection.to_owned(),
                timeline_key.to_owned(),
            ))
            .cloned())
    }

    async fn put_projection_checkpoint(
        &self,
        checkpoint: SessionProjectionCheckpoint,
    ) -> Result<(), HaiderError> {
        let boundary_matches = self
            .sessions
            .lock()
            .await
            .get(&checkpoint.session_id)
            .and_then(|events| {
                events
                    .iter()
                    .find(|event| event.seq == checkpoint.through_seq)
            })
            .is_some_and(|event| event.event_id == checkpoint.boundary_event_id);
        if !boundary_matches {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "projection checkpoint does not name its immutable boundary event",
                false,
            ));
        }
        let key = (
            checkpoint.session_id.clone(),
            checkpoint.projection.clone(),
            checkpoint.timeline_key.clone(),
        );
        let mut checkpoints = self.checkpoints.lock().await;
        if checkpoints
            .get(&key)
            .is_none_or(|current| current.through_seq <= checkpoint.through_seq)
        {
            checkpoints.insert(key, checkpoint);
        }
        Ok(())
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        let Some(mut current) = branch_id.cloned() else {
            return Ok(Vec::new());
        };
        let sessions = self.sessions.lock().await;
        let journal = sessions
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut descriptors = HashMap::<BranchId, BranchDescriptor>::new();
        for envelope in journal {
            if let Some(created) = BranchCreated::from_payload_value(&envelope.payload) {
                descriptors.insert(created.branch.branch_id.clone(), created.branch);
                continue;
            }
            let Ok(payload) = envelope.payload.decode_event() else {
                continue;
            };
            match payload {
                EventPayload::NodeCommitted(node)
                    if envelope.agent_id.is_none() && envelope.branch_id.as_ref().is_some() =>
                {
                    if let Some(descriptor) = envelope
                        .branch_id
                        .as_ref()
                        .and_then(|branch| descriptors.get_mut(branch))
                    {
                        descriptor.head_node_id = node.node;
                        descriptor.head_seq = envelope.seq;
                    }
                }
                _ => {}
            }
        }
        let mut reverse = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return Err(HaiderError::new(
                    ErrorCode::StoreCorrupt,
                    format!("branch registry contains a cycle at {current}"),
                    false,
                ));
            }
            let descriptor = descriptors.get(&current).cloned().ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::InvalidArgument,
                    format!("branch {current} does not exist"),
                    false,
                )
            })?;
            let source = descriptor.source_branch_id.clone();
            reverse.push(descriptor);
            let Some(source) = source else {
                break;
            };
            current = source;
        }
        reverse.reverse();
        Ok(reverse)
    }
}
