//! SQLite-backed event journal. Owns the sequence-allocation law:
//!
//! - `seq` is per-session, starts at 1, and is allocated only at commit time
//!   as `MAX(seq) + 1` inside an IMMEDIATE transaction, so committed
//!   sequences are monotonic and gap-free even across processes.
//! - An envelope is TRUE only once [`EventStore::append`] returns. Publishing
//!   committed envelopes to live subscribers is the caller's duty.
//! - The `envelope_json` column is the authoritative byte-for-byte record;
//!   the `seq` / `event_id` / `committed_at_ms` columns are denormalized
//!   copies for indexing, cross-checked against the JSON on every read.
//! - `worker_generation` is profile-owned and advances once per successful
//!   open while the exclusive profile lock is held, fencing actor identities
//!   across process restarts even when the wall clock repeats.

use crate::cas::FileCas;
use crate::migrations;
use crate::profile_lock::ProfileLock;
use crate::{Cas, StoreResult, now_ms, store_error, to_sqlite_integer};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{ArtifactRef, SessionId};
use rusqlite::{
    Connection, Error as SqliteError, ErrorCode as SqliteErrorCode, TransactionBehavior, params,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const REPLAY_PAGE_SIZE: usize = 1_024;

/// The inclusive sequence range allocated by one atomic append.
///
/// [`EventStore::append`] rejects empty batches, so it never returns an empty
/// range; `is_empty` exists for ranges constructed elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSeqRange {
    pub session_id: SessionId,
    pub first_seq: u64,
    pub last_seq: u64,
}

impl CommittedSeqRange {
    pub fn len(&self) -> u64 {
        if self.is_empty() {
            0
        } else {
            self.last_seq - self.first_seq + 1
        }
    }

    pub fn is_empty(&self) -> bool {
        self.first_seq > self.last_seq
    }
}

/// Synchronous durability port for the committed event stream.
pub trait EventStore: Send + Sync {
    /// Atomically appends one same-session batch.
    ///
    /// Sequence and commit-time fields are assigned at commit. The caller's
    /// envelopes are updated only after the transaction succeeds, making them
    /// safe to publish once this method returns. Empty and mixed-session
    /// batches are rejected with `InvalidArgument`.
    fn append(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange>;

    /// Reads committed envelopes with `seq > since_seq`, ordered by sequence.
    fn read(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> StoreResult<Vec<RawEnvelope>>;

    /// Returns the latest committed sequence, or zero for an empty session.
    fn latest_seq(&self, session: &SessionId) -> StoreResult<u64>;
}

/// A locked profile containing a SQLite event journal and filesystem CAS.
///
/// One connection lives for the full profile lifetime. A mutex serializes its
/// synchronous journal calls, while SQLite's statement cache avoids preparing
/// the hot append/read queries again on every event.
pub struct Store {
    root: PathBuf,
    database_path: PathBuf,
    worker_generation: u64,
    connection: Mutex<Connection>,
    cas: FileCas,
    _lock: ProfileLock,
}

impl Store {
    /// Opens or creates a durable profile at `root`.
    pub fn open(root: impl AsRef<Path>) -> StoreResult<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|error| {
            store_error(
                ErrorCode::Internal,
                format!("cannot create store root {}: {error}", root.display()),
                false,
            )
        })?;
        let profile_lock = ProfileLock::acquire(&root)?;
        let database_path = root.join("store.sqlite");
        let mut connection = open_connection(&database_path)?;
        migrations::migrate(&mut connection)?;
        connection.set_prepared_statement_cache_capacity(16);
        let cas = FileCas::open(&root)?;
        let worker_generation = next_worker_generation(&mut connection)?;

        Ok(Self {
            root,
            database_path,
            worker_generation,
            connection: Mutex::new(connection),
            cas,
            _lock: profile_lock,
        })
    }

    /// The profile root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The SQLite database path.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Durable fencing generation allocated by this successful profile open.
    pub fn worker_generation(&self) -> u64 {
        self.worker_generation
    }

    /// The profile's content-addressed storage.
    pub fn cas(&self) -> &FileCas {
        &self.cas
    }

    /// Current supported and migrated SQLite schema version.
    pub fn schema_version(&self) -> StoreResult<u32> {
        let connection = self.connection()?;
        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_sqlite_error)?;
        Ok(version)
    }

    /// Replays a session's complete journal in committed sequence order.
    pub fn journal_replay(&self, session: &SessionId) -> StoreResult<Vec<RawEnvelope>> {
        let mut replay = Vec::new();
        let mut since_seq = 0;
        loop {
            let page = self.read(session, since_seq, REPLAY_PAGE_SIZE)?;
            if page.is_empty() {
                return Ok(replay);
            }
            since_seq = page.last().map_or(since_seq, |envelope| envelope.seq);
            replay.extend(page);
        }
    }

    fn connection(&self) -> StoreResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| {
            store_error(
                ErrorCode::Internal,
                "SQLite journal connection lock is poisoned",
                false,
            )
        })
    }
}

/// Advances and returns the profile-owned fencing generation.
///
/// `Store::open` calls this only after acquiring the exclusive profile lock
/// and after every other fallible setup step, so each successful open consumes
/// exactly one generation.
fn next_worker_generation(connection: &mut Connection) -> StoreResult<u64> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite_error)?;
    let current: i64 = transaction
        .query_row(
            "SELECT worker_generation FROM profile_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| corrupt("worker generation space is exhausted"))?;
    let updated = transaction
        .execute(
            "UPDATE profile_meta
             SET worker_generation = ?1
             WHERE singleton = 1 AND worker_generation = ?2",
            params![next, current],
        )
        .map_err(map_sqlite_error)?;
    if updated != 1 {
        return Err(corrupt(
            "profile metadata is missing its worker generation singleton",
        ));
    }
    transaction.commit().map_err(map_sqlite_error)?;
    u64::try_from(next).map_err(|_| corrupt("database contains a negative worker generation"))
}

impl EventStore for Store {
    fn append(&self, envelopes: &mut [RawEnvelope]) -> StoreResult<CommittedSeqRange> {
        let (session, batch_len) = same_session_batch(envelopes)?;

        let mut connection = self.connection()?;
        // IMMEDIATE takes SQLite's write lock up front, so reading MAX(seq)
        // and inserting the batch form one critical section. Concurrent
        // appenders serialize here; that is what keeps allocated sequences
        // monotonic and gap-free.
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let committed_at_ms = now_ms()?;
        let committed_at_sql = to_sqlite_integer(committed_at_ms)?;
        transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
            )
            .and_then(|mut statement| {
                statement.execute(params![session.as_str(), committed_at_sql, "{}"])
            })
            .map_err(map_sqlite_error)?;
        let latest: i64 = transaction
            .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1")
            .and_then(|mut statement| statement.query_row([session.as_str()], |row| row.get(0)))
            .map_err(map_sqlite_error)?;
        let first_seq = u64::try_from(latest)
            .map_err(|_| corrupt("database contains a negative event sequence"))?
            .checked_add(1)
            .ok_or_else(|| corrupt("event sequence space is exhausted"))?;
        let last_seq = first_seq
            .checked_add(batch_len - 1)
            .ok_or_else(|| corrupt("event sequence space is exhausted"))?;

        // Stamp clones, not the caller's envelopes: if anything below fails,
        // the transaction rolls back and the caller's batch must be exactly
        // as it was passed in.
        let mut stamped = Vec::with_capacity(envelopes.len());
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO events(
                            session_id, seq, envelope_json, event_id, committed_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(map_sqlite_error)?;
            for (seq, envelope) in (first_seq..=last_seq).zip(envelopes.iter()) {
                let mut envelope = envelope.clone();
                envelope.seq = seq;
                envelope.committed_at_ms = committed_at_ms;
                let envelope_json = serde_json::to_string(&envelope).map_err(|error| {
                    store_error(
                        ErrorCode::InvalidArgument,
                        format!("cannot serialize event envelope: {error}"),
                        false,
                    )
                })?;
                insert
                    .execute(params![
                        session.as_str(),
                        to_sqlite_integer(seq)?,
                        envelope_json,
                        envelope.event_id.as_str(),
                        committed_at_sql,
                    ])
                    .map_err(map_sqlite_error)?;
                stamped.push(envelope);
            }
        }

        transaction.commit().map_err(map_sqlite_error)?;
        // The batch is durable now; reflect the committed fields to the caller.
        for (envelope, stamped) in envelopes.iter_mut().zip(stamped) {
            *envelope = stamped;
        }
        Ok(CommittedSeqRange {
            session_id: session,
            first_seq,
            last_seq,
        })
    }

    fn read(
        &self,
        session: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> StoreResult<Vec<RawEnvelope>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        // A limit beyond i64::MAX is effectively unbounded; clamp, don't error.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection
            .prepare_cached(
                "SELECT seq, envelope_json, event_id, committed_at_ms
                 FROM events
                 WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC
                 LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement
            .query(params![
                session.as_str(),
                to_sqlite_integer(since_seq)?,
                limit
            ])
            .map_err(map_sqlite_error)?;
        let mut envelopes = Vec::new();
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            let stored_seq: i64 = row.get(0).map_err(map_sqlite_error)?;
            let envelope_json: String = row.get(1).map_err(map_sqlite_error)?;
            let stored_event_id: String = row.get(2).map_err(map_sqlite_error)?;
            let stored_committed_at_ms: i64 = row.get(3).map_err(map_sqlite_error)?;
            let envelope: RawEnvelope = serde_json::from_str(&envelope_json).map_err(|error| {
                corrupt(format!(
                    "invalid envelope JSON for session {session}, seq {stored_seq}: {error}"
                ))
            })?;
            validate_stored_envelope(
                session,
                stored_seq,
                &stored_event_id,
                stored_committed_at_ms,
                &envelope,
            )?;
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    fn latest_seq(&self, session: &SessionId) -> StoreResult<u64> {
        let connection = self.connection()?;
        let latest: i64 = connection
            .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1")
            .and_then(|mut statement| statement.query_row([session.as_str()], |row| row.get(0)))
            .map_err(map_sqlite_error)?;
        u64::try_from(latest).map_err(|_| corrupt("database contains a negative event sequence"))
    }
}

impl Cas for Store {
    fn put(&self, bytes: &[u8]) -> StoreResult<ArtifactRef> {
        self.cas.put(bytes)
    }

    fn put_file(&self, path: &Path) -> StoreResult<ArtifactRef> {
        self.cas.put_file(path)
    }

    fn get(&self, artifact: &ArtifactRef) -> StoreResult<Vec<u8>> {
        self.cas.get(artifact)
    }

    fn verify(&self, artifact: &ArtifactRef) -> bool {
        self.cas.verify(artifact)
    }
}

/// Validates an append batch: non-empty and single-session.
/// Returns the batch's session and its length as a u64.
fn same_session_batch(envelopes: &[RawEnvelope]) -> StoreResult<(SessionId, u64)> {
    let Some(first) = envelopes.first() else {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "cannot append an empty envelope batch",
            false,
        ));
    };
    let session = first.session_id.clone();
    if envelopes
        .iter()
        .any(|envelope| envelope.session_id != session)
    {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            "one append batch cannot span multiple sessions",
            false,
        ));
    }
    let batch_len = u64::try_from(envelopes.len()).map_err(|_| {
        store_error(
            ErrorCode::InvalidArgument,
            "envelope batch is too large",
            false,
        )
    })?;
    Ok((session, batch_len))
}

/// Opens the profile's long-lived journal connection with the required pragmas
/// (WAL, FULL synchronous, foreign keys, busy timeout).
fn open_connection(path: &Path) -> StoreResult<Connection> {
    let connection = Connection::open(path).map_err(map_sqlite_error)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_error)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(map_sqlite_error)?;
    Ok(connection)
}

/// Cross-checks the denormalized row columns against the fields embedded in
/// the envelope JSON. Any disagreement means the journal was tampered with or
/// corrupted, never a validation bug in the caller.
fn validate_stored_envelope(
    requested_session: &SessionId,
    stored_seq: i64,
    stored_event_id: &str,
    stored_committed_at_ms: i64,
    envelope: &RawEnvelope,
) -> StoreResult<()> {
    let seq = u64::try_from(stored_seq)
        .map_err(|_| corrupt("database contains a negative event sequence"))?;
    let committed_at_ms = u64::try_from(stored_committed_at_ms)
        .map_err(|_| corrupt("database contains a negative commit timestamp"))?;
    if envelope.session_id != *requested_session
        || envelope.seq != seq
        || envelope.event_id.as_str() != stored_event_id
        || envelope.committed_at_ms != committed_at_ms
    {
        return Err(corrupt(format!(
            "event row and envelope disagree for session {requested_session}, seq {stored_seq}"
        )));
    }
    Ok(())
}

/// Maps SQLite failure classes onto protocol error codes: busy/locked becomes
/// retryable `StoreLocked`, constraint violations become `InvalidArgument`
/// (the caller sent conflicting data, e.g. a duplicate event ID), and
/// corrupt-database classes become `StoreCorrupt`.
fn map_sqlite_error(error: SqliteError) -> HaiderError {
    match &error {
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::DatabaseBusy | SqliteErrorCode::DatabaseLocked
            ) =>
        {
            store_error(
                ErrorCode::StoreLocked,
                format!("SQLite journal is busy: {error}"),
                true,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(inner.code, SqliteErrorCode::ConstraintViolation) =>
        {
            store_error(
                ErrorCode::InvalidArgument,
                format!("event append violates a journal constraint: {error}"),
                false,
            )
        }
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                SqliteErrorCode::DatabaseCorrupt | SqliteErrorCode::NotADatabase
            ) =>
        {
            corrupt(format!("SQLite journal is corrupt: {error}"))
        }
        _ => store_error(
            ErrorCode::Internal,
            format!("SQLite journal operation failed: {error}"),
            false,
        ),
    }
}

fn corrupt(message: impl Into<String>) -> HaiderError {
    store_error(ErrorCode::StoreCorrupt, message, false)
}
