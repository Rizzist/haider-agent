//! Async adapter for the synchronous SQLite journal and filesystem CAS.
//!
//! Every potentially blocking profile, SQLite, mutex, and filesystem operation
//! runs on Tokio's blocking pool. The wrapped [`Store`] owns one connection and
//! the profile lock for the full lifetime of this handle.

use crate::{CommittedRange, StoreHandle};
use async_trait::async_trait;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_store::{Cas, EventStore, Store};
use std::path::Path;
use std::sync::Arc;

/// Async, cloneable owner of one locked, persistent SQLite profile.
#[derive(Clone)]
pub struct SqliteStoreHandle {
    store: Arc<Store>,
}

impl SqliteStoreHandle {
    /// Opens or creates `root` without blocking the calling runtime worker.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, HaiderError> {
        let root = root.as_ref().to_path_buf();
        let store = run_blocking(move || Store::open(root)).await?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    /// Durably stores artifact bytes on the blocking pool.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<ArtifactRef, HaiderError> {
        let store = Arc::clone(&self.store);
        run_blocking(move || store.put(&bytes)).await
    }

    /// Reads and verifies an artifact on the blocking pool.
    pub async fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        let store = Arc::clone(&self.store);
        let artifact = artifact.clone();
        run_blocking(move || store.get(&artifact)).await
    }

    /// Verifies an artifact on the blocking pool.
    pub async fn verify(&self, artifact: &ArtifactRef) -> Result<bool, HaiderError> {
        let store = Arc::clone(&self.store);
        let artifact = artifact.clone();
        run_blocking(move || Ok(store.verify(&artifact))).await
    }
}

#[async_trait]
impl StoreHandle for SqliteStoreHandle {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let store = Arc::clone(&self.store);
        let mut owned = envelopes.to_vec();
        let (range, committed) = run_blocking(move || {
            let range = store.append(&mut owned)?;
            Ok((range, owned))
        })
        .await?;

        envelopes.clone_from_slice(&committed);
        Ok(CommittedRange {
            first_seq: range.first_seq,
            last_seq: range.last_seq,
        })
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        let store = Arc::clone(&self.store);
        let session_id = session_id.clone();
        run_blocking(move || store.read(&session_id, since_seq, limit)).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        let store = Arc::clone(&self.store);
        let session_id = session_id.clone();
        run_blocking(move || store.latest_seq(&session_id)).await
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T, HaiderError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, HaiderError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            HaiderError::new(
                ErrorCode::Internal,
                format!("SQLite store blocking task failed: {error}"),
                false,
            )
        })?
}
