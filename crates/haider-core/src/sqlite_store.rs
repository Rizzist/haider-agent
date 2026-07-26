//! Async adapter for the synchronous SQLite journal and filesystem CAS.
//!
//! Every potentially blocking profile, SQLite, mutex, and filesystem operation
//! runs on Tokio's blocking pool. The wrapped [`Store`] owns one connection and
//! the profile lock until [`SqliteStoreHandle::close`] or final fallback drop.

use crate::{CommittedRange, StoreHandle};
use async_trait::async_trait;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{ArtifactRef, SessionId};
use haider_store::{Cas, EventStore, Store};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Async, cloneable owner of one locked, persistent SQLite profile.
///
/// Call [`close`](Self::close) on the owning path to move connection teardown,
/// WAL cleanup, and profile-lock release onto Tokio's blocking pool. If callers
/// omit it, dropping the final clone still closes the store synchronously as a
/// best-effort fallback.
#[derive(Clone)]
pub struct SqliteStoreHandle {
    owner: Arc<StoreOwner>,
}

struct StoreOwner {
    worker_generation: u64,
    store: Mutex<Option<Store>>,
}

impl SqliteStoreHandle {
    /// Opens or creates `root` without blocking the calling runtime worker.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, HaiderError> {
        let root = root.as_ref().to_path_buf();
        let store = run_blocking(move || Store::open(root)).await?;
        let worker_generation = store.worker_generation();
        Ok(Self {
            owner: Arc::new(StoreOwner {
                worker_generation,
                store: Mutex::new(Some(store)),
            }),
        })
    }

    /// Profile-owned fencing generation allocated by this store open.
    pub fn worker_generation(&self) -> u64 {
        self.owner.worker_generation
    }

    /// Closes the SQLite connection and releases the profile lock off runtime
    /// workers. All clones share the close state; calls after close fail.
    pub async fn close(self) -> Result<(), HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || {
            let store = owner.take_store()?;
            drop(store);
            Ok(())
        })
        .await
    }

    /// Durably stores artifact bytes on the blocking pool.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<ArtifactRef, HaiderError> {
        let owner = Arc::clone(&self.owner);
        run_blocking(move || owner.with_store(|store| store.put(&bytes))).await
    }

    /// Reads and verifies an artifact on the blocking pool.
    pub async fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let artifact = artifact.clone();
        run_blocking(move || owner.with_store(|store| store.get(&artifact))).await
    }

    /// Verifies an artifact on the blocking pool.
    pub async fn verify(&self, artifact: &ArtifactRef) -> Result<bool, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let artifact = artifact.clone();
        run_blocking(move || owner.with_store(|store| Ok(store.verify(&artifact)))).await
    }
}

#[async_trait]
impl StoreHandle for SqliteStoreHandle {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let mut owned = envelopes.to_vec();
        let (range, committed) = run_blocking(move || {
            owner.with_store(|store| {
                let range = store.append(&mut owned)?;
                Ok((range, owned))
            })
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
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.read(&session_id, since_seq, limit)))
            .await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        let owner = Arc::clone(&self.owner);
        let session_id = session_id.clone();
        run_blocking(move || owner.with_store(|store| store.latest_seq(&session_id))).await
    }
}

impl StoreOwner {
    fn with_store<T>(
        &self,
        operation: impl FnOnce(&Store) -> Result<T, HaiderError>,
    ) -> Result<T, HaiderError> {
        let store = self.store.lock().map_err(|_| owner_lock_error())?;
        let store = store.as_ref().ok_or_else(closed_error)?;
        operation(store)
    }

    fn take_store(&self) -> Result<Option<Store>, HaiderError> {
        self.store
            .lock()
            .map(|mut store| store.take())
            .map_err(|_| owner_lock_error())
    }
}

fn owner_lock_error() -> HaiderError {
    HaiderError::new(
        ErrorCode::Internal,
        "SQLite store owner lock is poisoned",
        false,
    )
}

fn closed_error() -> HaiderError {
    HaiderError::new(ErrorCode::Internal, "SQLite store handle is closed", false)
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
