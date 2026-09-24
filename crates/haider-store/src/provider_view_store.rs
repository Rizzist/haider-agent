//! Disk-only storage for exact provider-rendered cache prefixes.
//!
//! SQLite owns request/block coordinates and expiry. A dedicated filesystem
//! CAS owns immutable bytes so sweeping this index can never remove an
//! unrelated attachment artifact with coincidentally identical contents.
//! Resident state is limited to hashes/lengths plus a byte-capped hot-block
//! LRU; complete request views are never cached or memory-mapped.

use crate::cas::FileCas;
use crate::event_store::map_sqlite_error;
use crate::{Cas, StoreResult, now_ms, store_error, to_sqlite_integer};
use haider_protocol::cache::{
    ProviderViewBlobV1, ProviderViewBlockRefV1, ProviderViewLedgerV1, ProviderViewStorageV1,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{ArtifactRef, SessionId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "provider_view_store_tests.rs"]
mod provider_view_store_tests;

pub(crate) const PROVIDER_VIEW_HOT_BLOCK_CAP_BYTES: usize = 64 * 1024;
pub(crate) const PROVIDER_VIEW_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const PROVIDER_VIEW_SWEEP_REQUEST_BATCH: usize = 128;
const PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL: u64 = 64;
const PROVIDER_VIEW_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(crate) struct ProviderViewStore {
    cas: FileCas,
    hot_blocks: Mutex<HotBlockLru>,
    sweep_schedule: Mutex<ProviderViewSweepSchedule>,
}

pub(crate) struct PreparedProviderView {
    ledger: ProviderViewLedgerV1,
    expected: HashSet<ProviderViewBlockRefV1>,
}

/// Domain separator for [`history_digest`]. Persisted in request cursors and
/// compact journal records, so changing it invalidates existing profiles.
const HISTORY_DIGEST_DOMAIN: &[u8] = b"haider.provider-view.history-prefix.v1\0";

/// Digest of an ordered history ledger (hash and length of every block). It
/// binds a request cursor or compact journal record to its exact ledger.
pub(crate) fn history_digest(blocks: &[ProviderViewBlockRefV1]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(HISTORY_DIGEST_DOMAIN);
    for block in blocks {
        hasher.update(block.content_hash.as_bytes());
        hasher.update(&block.byte_len.to_be_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Read one immutable history prefix. Segment rows are never removed: journal
/// attempt facts can still be decoded after the seven-day CAS/index expiry.
pub(crate) fn history_segment_prefix(
    connection: &Connection,
    session_id: &SessionId,
    segment_id: i64,
    count: usize,
) -> StoreResult<Vec<ProviderViewBlockRefV1>> {
    let owner: Option<String> = connection
        .query_row(
            "SELECT session_id FROM provider_view_history_segments WHERE id = ?1",
            [segment_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_read_error)?;
    if owner.as_deref() != Some(session_id.as_str()) {
        return Err(corrupt(
            "provider-view history segment belongs to another session",
        ));
    }
    let mut statement = connection
        .prepare_cached(
            "WITH RECURSIVE chain(segment_id, cutoff) AS (
                 SELECT ?1, ?2
                 UNION ALL
                 SELECT s.parent_segment_id, MIN(chain.cutoff, s.parent_block_count)
                 FROM chain JOIN provider_view_history_segments s ON s.id = chain.segment_id
                 WHERE s.parent_segment_id IS NOT NULL
             )
             SELECT s.session_id, b.block_ordinal, b.content_hash, b.byte_len
             FROM chain
             JOIN provider_view_history_segments s ON s.id = chain.segment_id
             LEFT JOIN provider_view_history_blocks b
               ON b.segment_id = s.id
              AND b.block_ordinal >= s.parent_block_count
              AND b.block_ordinal < chain.cutoff
             ORDER BY b.block_ordinal",
        )
        .map_err(sqlite_read_error)?;
    let rows = statement
        .query_map(
            params![
                segment_id,
                i64::try_from(count)
                    .map_err(|_| corrupt("history prefix exceeds SQLite integer space"))?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(sqlite_read_error)?;
    let mut blocks = Vec::with_capacity(count);
    for row in rows {
        let (owner, ordinal, content_hash, byte_len) = row.map_err(sqlite_read_error)?;
        if owner != session_id.as_str() {
            return Err(corrupt(
                "provider-view history segment belongs to another session",
            ));
        }
        let Some(ordinal) = ordinal else { continue };
        let (Some(content_hash), Some(byte_len)) = (content_hash, byte_len) else {
            return Err(corrupt("provider-view history segment row is incomplete"));
        };
        if ordinal != i64::try_from(blocks.len()).unwrap_or(i64::MAX) {
            return Err(corrupt(
                "provider-view history segment has a missing ordinal",
            ));
        }
        blocks.push(ProviderViewBlockRefV1 {
            content_hash,
            byte_len: u64::try_from(byte_len)
                .map_err(|_| corrupt("provider-view history block length is negative"))?,
        });
    }
    if blocks.len() != count {
        return Err(corrupt(
            "provider-view history segment is shorter than its request",
        ));
    }
    Ok(blocks)
}

/// Where one request's history ledger lives in the trunk/leaf segment tree.
///
/// Invariants (schema v32):
/// - Segment rows and block rows are immutable once a later row refers to
///   them. A trunk only grows past its current end; a leaf never grows.
/// - A segment's own blocks occupy ordinals `parent_block_count..`; ordinals
///   below `parent_block_count` are read from the parent, cut at that count.
///   Parents always have smaller ids, so every chain is finite.
/// - Each request owns one leaf whose parent cutoff equals its trunk's length
///   when the leaf was created (`prefix_len`). Later trunk growth is therefore
///   invisible to older leaves, which keeps journal replay deterministic.
/// - A trunk holds only blocks that two consecutive requests agreed on, so a
///   request writes O(changed tail) rows, never its whole history again.
struct HistoryPlacement {
    trunk_id: i64,
    /// Trunk length before this request; `trunk_end..prefix_len` is appended.
    trunk_end: usize,
    leaf_id: i64,
    /// Blocks shared with the trunk; `prefix_len..` is stored in the leaf.
    prefix_len: usize,
}

/// Chooses or creates this request's trunk and inserts its new leaf segment.
/// The caller inserts the block rows from the returned ranges. Each request
/// gets a short immutable leaf. Its parent trunk contains blocks confirmed
/// stable by the next request and only grows by the newly confirmed delta.
/// When an earlier stable block changes, a new trunk branches from the common
/// prefix (or starts empty), leaving the old trunk and its leaves untouched.
fn place_history_segments(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &SessionId,
    history_blocks: &[ProviderViewBlockRefV1],
) -> StoreResult<HistoryPlacement> {
    let previous: Option<(Option<i64>, Option<i64>)> = transaction
        .query_row(
            "SELECT h.segment_id, h.block_count
             FROM provider_view_requests r
             LEFT JOIN provider_view_request_history h
               ON h.session_id = r.session_id
              AND h.request_ordinal = r.request_ordinal
             WHERE r.session_id = ?1
             ORDER BY r.request_ordinal DESC LIMIT 1",
            [session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_read_error)?;
    let (trunk_id, trunk_end, prefix_len) = match previous {
        Some((Some(leaf_id), Some(count))) => {
            let count = usize::try_from(count)
                .map_err(|_| corrupt("provider-view history count is negative"))?;
            let earlier = history_segment_prefix(transaction, session_id, leaf_id, count)?;
            let common = earlier
                .iter()
                .zip(history_blocks)
                .take_while(|(left, right)| left == right)
                .count();
            let trunk_id: i64 = transaction
                .query_row(
                    "SELECT parent_segment_id FROM provider_view_history_segments
                     WHERE id = ?1",
                    [leaf_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(sqlite_read_error)?
                .ok_or_else(|| corrupt("provider-view history leaf has no trunk"))?;
            let (parent_count, local_end): (i64, i64) = transaction
                .query_row(
                    "SELECT s.parent_block_count, COALESCE(MAX(b.block_ordinal) + 1, 0)
                     FROM provider_view_history_segments s
                     LEFT JOIN provider_view_history_blocks b ON b.segment_id = s.id
                     WHERE s.id = ?1 GROUP BY s.id",
                    [trunk_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(sqlite_read_error)?;
            let trunk_end = usize::try_from(parent_count.max(local_end))
                .map_err(|_| corrupt("provider-view trunk length is negative"))?;
            if common >= trunk_end {
                (trunk_id, trunk_end, common)
            } else {
                let new_trunk = if common == 0 {
                    transaction
                        .execute(
                            "INSERT INTO provider_view_history_segments(session_id)
                             VALUES (?1)",
                            [session_id.as_str()],
                        )
                        .map_err(sqlite_write_error)?;
                    transaction.last_insert_rowid()
                } else {
                    transaction
                        .execute(
                            "INSERT INTO provider_view_history_segments(
                                session_id, parent_segment_id, parent_block_count
                             ) VALUES (?1, ?2, ?3)",
                            params![
                                session_id.as_str(),
                                trunk_id,
                                i64::try_from(common).map_err(|_| invalid(
                                    "provider-view history prefix exceeds SQLite integer space"
                                ))?,
                            ],
                        )
                        .map_err(sqlite_write_error)?;
                    transaction.last_insert_rowid()
                };
                (new_trunk, common, common)
            }
        }
        _ => {
            transaction
                .execute(
                    "INSERT INTO provider_view_history_segments(session_id) VALUES (?1)",
                    [session_id.as_str()],
                )
                .map_err(sqlite_write_error)?;
            (transaction.last_insert_rowid(), 0, 0)
        }
    };
    transaction
        .execute(
            "INSERT INTO provider_view_history_segments(
                session_id, parent_segment_id, parent_block_count
             ) VALUES (?1, ?2, ?3)",
            params![
                session_id.as_str(),
                trunk_id,
                i64::try_from(prefix_len).map_err(|_| invalid(
                    "provider-view history prefix exceeds SQLite integer space"
                ))?,
            ],
        )
        .map_err(sqlite_write_error)?;
    Ok(HistoryPlacement {
        trunk_id,
        trunk_end,
        leaf_id: transaction.last_insert_rowid(),
        prefix_len,
    })
}

/// Inserts `blocks` into `segment_id` at ordinals starting from `start`.
fn insert_history_blocks(
    transaction: &rusqlite::Transaction<'_>,
    segment_id: i64,
    start: usize,
    blocks: &[ProviderViewBlockRefV1],
) -> StoreResult<()> {
    for (offset, block) in blocks.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO provider_view_history_blocks(
                    segment_id, block_ordinal, content_hash, byte_len
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    segment_id,
                    i64::try_from(start + offset).map_err(|_| invalid(
                        "provider-view history ordinal exceeds SQLite integer space"
                    ))?,
                    &block.content_hash,
                    to_sqlite_integer(block.byte_len)?,
                ],
            )
            .map_err(sqlite_write_error)?;
    }
    Ok(())
}

/// Monotonic provider-view maintenance watermark. The count bound prevents a
/// busy long-lived daemon from postponing cleanup indefinitely, while the time
/// bound ensures a quiet profile checks expiry on its next provider persist.
struct ProviderViewSweepSchedule {
    persists_since_sweep: u64,
    next_sweep_at: Instant,
}

impl ProviderViewSweepSchedule {
    fn after_sweep(now: Instant) -> Self {
        Self {
            persists_since_sweep: 0,
            next_sweep_at: now + PROVIDER_VIEW_SWEEP_INTERVAL,
        }
    }

    fn note_persist_and_is_due(&mut self, now: Instant) -> bool {
        self.persists_since_sweep = self.persists_since_sweep.saturating_add(1);
        self.persists_since_sweep >= PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL
            || now >= self.next_sweep_at
    }

    fn sweep_completed(&mut self, now: Instant) {
        *self = Self::after_sweep(now);
    }
}

impl ProviderViewStore {
    pub(crate) fn open(profile_root: &Path) -> StoreResult<Self> {
        Self::open_with_publication(profile_root, true)
    }

    pub(crate) fn open_deferred(profile_root: &Path) -> StoreResult<Self> {
        Self::open_with_publication(profile_root, false)
    }

    fn open_with_publication(profile_root: &Path, publish_created: bool) -> StoreResult<Self> {
        let cas = if publish_created {
            FileCas::open_namespace(profile_root, "provider-view-cas")?
        } else {
            FileCas::open_namespace_deferred(profile_root, "provider-view-cas")?
        };
        #[cfg(target_vendor = "apple")]
        ensure_provider_view_barrier_device(profile_root)?;
        Ok(Self {
            cas,
            hot_blocks: Mutex::new(HotBlockLru::new(PROVIDER_VIEW_HOT_BLOCK_CAP_BYTES)),
            sweep_schedule: Mutex::new(ProviderViewSweepSchedule::after_sweep(Instant::now())),
        })
    }

    pub(crate) fn persist(
        &self,
        connection: &mut Connection,
        session_id: &SessionId,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
        expires_at_ms: u64,
    ) -> StoreResult<ProviderViewLedgerV1> {
        let prepared = self.prepare(ledger, blobs)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_write_error)?;
        let ledger = self.persist_prepared(&transaction, session_id, prepared, expires_at_ms)?;
        transaction.commit().map_err(sqlite_write_error)?;
        Ok(ledger)
    }

    /// Validates and ordering-fences the immutable CAS bytes. No SQLite index
    /// reference is visible until a caller publishes [`PreparedProviderView`]
    /// in its chosen transaction. The fence guarantees that a surviving index
    /// cannot precede its blocks; it does not independently guarantee that the
    /// blocks have reached persistent media when this method returns.
    pub(crate) fn prepare(
        &self,
        ledger: ProviderViewLedgerV1,
        blobs: Vec<ProviderViewBlobV1>,
    ) -> StoreResult<PreparedProviderView> {
        if ledger.storage.is_some() {
            return Err(invalid("provider-view ledger is already storage-addressed"));
        }
        let expected = ledger_block_refs(&ledger)
            .into_iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut missing = expected.clone();
        for blob in &blobs {
            if !missing.remove(&blob.block) && !expected.contains(&blob.block) {
                return Err(invalid(
                    "provider-view blob is not referenced by the ledger",
                ));
            }
        }
        if !missing.is_empty() {
            return Err(invalid(
                "provider-view write omitted one or more referenced blocks",
            ));
        }

        let mut persisted = HashSet::new();
        for blob in blobs {
            if persisted.insert(blob.block.clone()) {
                let expected = ArtifactRef::new(blob.block.content_hash.clone());
                let stored =
                    self.cas
                        .put_batched_stream(&expected, blob.block.byte_len, |writer| {
                            blob.write_to(writer)
                        })?;
                if stored.as_str() != blob.block.content_hash {
                    return Err(corrupt(
                        "provider-view CAS returned a different content address",
                    ));
                }
            }
        }
        // Every blob is plain-fsynced before this one device barrier and the
        // index transaction.
        self.cas.finish_ordered_batched_puts()?;

        Ok(PreparedProviderView { ledger, expected })
    }

    /// Publishes the CAS index inside the caller's transaction. The caller may
    /// atomically append the journal attempt facts before committing it.
    pub(crate) fn persist_prepared(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        session_id: &SessionId,
        prepared: PreparedProviderView,
        expires_at_ms: u64,
    ) -> StoreResult<ProviderViewLedgerV1> {
        let PreparedProviderView {
            mut ledger,
            expected,
        } = prepared;
        let HistoryPlacement {
            trunk_id,
            trunk_end,
            leaf_id: segment_id,
            prefix_len,
        } = place_history_segments(transaction, session_id, &ledger.history_blocks)?;
        let newly_stable = &ledger.history_blocks[trunk_end..prefix_len];
        let suffix = &ledger.history_blocks[prefix_len..];
        let mut gc_candidates = HashSet::from([
            ledger.system_block.clone(),
            ledger.tool_schema_block.clone(),
        ]);
        gc_candidates.extend(newly_stable.iter().chain(suffix).cloned());
        debug_assert!(gc_candidates.is_subset(&expected));
        self.queue_gc(transaction, &gc_candidates)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO provider_view_session_cursors(
                    session_id, next_request_ordinal
                 ) SELECT ?1, COALESCE(MAX(request_ordinal), 0) + 1
                   FROM provider_view_requests WHERE session_id = ?1",
                [session_id.as_str()],
            )
            .map_err(sqlite_write_error)?;
        let request_ordinal: i64 = transaction
            .query_row(
                "SELECT next_request_ordinal FROM provider_view_session_cursors
                 WHERE session_id = ?1",
                [session_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sqlite_write_error)?;
        let next_request_ordinal = request_ordinal.checked_add(1).ok_or_else(|| {
            corrupt("provider-view request ordinal exhausted SQLite integer space")
        })?;
        transaction
            .execute(
                "UPDATE provider_view_session_cursors
                 SET next_request_ordinal = ?2 WHERE session_id = ?1",
                params![session_id.as_str(), next_request_ordinal],
            )
            .map_err(sqlite_write_error)?;
        transaction
            .execute(
                "INSERT INTO provider_view_requests(
                    session_id, request_ordinal, provider, model,
                    cache_epoch, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id.as_str(),
                    request_ordinal,
                    &ledger.provider,
                    &ledger.model,
                    &ledger.cache_epoch,
                    to_sqlite_integer(expires_at_ms)?,
                ],
            )
            .map_err(sqlite_write_error)?;
        transaction
            .execute(
                "INSERT INTO provider_view_request_history(
                    session_id, request_ordinal, segment_id, block_count, history_digest
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id.as_str(),
                    request_ordinal,
                    segment_id,
                    i64::try_from(ledger.history_blocks.len()).map_err(|_| invalid(
                        "provider-view history exceeds SQLite integer space"
                    ))?,
                    history_digest(&ledger.history_blocks),
                ],
            )
            .map_err(sqlite_write_error)?;
        insert_block(
            transaction,
            &ledger,
            session_id,
            request_ordinal,
            "system",
            0,
            &ledger.system_block,
            expires_at_ms,
        )?;
        insert_block(
            transaction,
            &ledger,
            session_id,
            request_ordinal,
            "tools",
            0,
            &ledger.tool_schema_block,
            expires_at_ms,
        )?;
        insert_history_blocks(transaction, trunk_id, trunk_end, newly_stable)?;
        insert_history_blocks(transaction, segment_id, prefix_len, suffix)?;
        {
            let mut delete_gc = transaction
                .prepare_cached("DELETE FROM provider_view_gc WHERE content_hash = ?1")
                .map_err(sqlite_write_error)?;
            for block in &gc_candidates {
                delete_gc
                    .execute([&block.content_hash])
                    .map_err(sqlite_write_error)?;
            }
        }
        ledger.storage = Some(ProviderViewStorageV1 {
            session_id: session_id.clone(),
            request_ordinal: u64::try_from(request_ordinal)
                .map_err(|_| corrupt("provider-view request ordinal is negative"))?,
            expires_at_ms,
        });
        Ok(ledger)
    }

    pub(crate) fn verify(
        &self,
        connection: &Connection,
        ledger: &ProviderViewLedgerV1,
    ) -> StoreResult<()> {
        let storage = ledger
            .storage
            .as_ref()
            .ok_or_else(|| corrupt("durable provider-view ledger has no disk storage cursor"))?;
        let request_ordinal = to_sqlite_integer(storage.request_ordinal)?;
        let request = connection
            .query_row(
                "SELECT r.provider, r.model, r.cache_epoch, r.expires_at_ms,
                        h.segment_id, h.block_count, h.history_digest
                 FROM provider_view_requests r
                 LEFT JOIN provider_view_request_history h
                   ON h.session_id = r.session_id
                  AND h.request_ordinal = r.request_ordinal
                 WHERE r.session_id = ?1 AND r.request_ordinal = ?2",
                params![storage.session_id.as_str(), request_ordinal],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_read_error)?
            .ok_or_else(|| corrupt("provider-view storage cursor is missing or expired"))?;
        let expires_at_ms =
            u64::try_from(request.3).map_err(|_| corrupt("provider-view expiry is negative"))?;
        if request.0 != ledger.provider
            || request.1 != ledger.model
            || request.2 != ledger.cache_epoch
            || expires_at_ms != storage.expires_at_ms
        {
            return Err(corrupt(
                "provider-view request index does not match its durable ledger",
            ));
        }

        let mut statement = connection
            .prepare_cached(
                "SELECT section, block_ordinal, content_hash, byte_len, expires_at_ms
                 FROM provider_view_blocks
                 WHERE session_id = ?1 AND request_ordinal = ?2
                 ORDER BY CASE section
                     WHEN 'system' THEN 0 WHEN 'tools' THEN 1 ELSE 2 END,
                     block_ordinal",
            )
            .map_err(sqlite_read_error)?;
        let mut stored = statement
            .query_map(
                params![storage.session_id.as_str(), request_ordinal],
                |row| {
                    Ok(StoredBlock {
                        section: row.get(0)?,
                        ordinal: row.get(1)?,
                        block: ProviderViewBlockRefV1 {
                            content_hash: row.get(2)?,
                            byte_len: u64::try_from(row.get::<_, i64>(3)?)
                                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, -1))?,
                        },
                        expires_at_ms: u64::try_from(row.get::<_, i64>(4)?)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, -1))?,
                    })
                },
            )
            .map_err(sqlite_read_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_read_error)?;
        if let (Some(segment_id), Some(count)) = (request.4, request.5) {
            let count = usize::try_from(count)
                .map_err(|_| corrupt("provider-view history count is negative"))?;
            if count != ledger.history_blocks.len()
                || request.6.as_deref() != Some(history_digest(&ledger.history_blocks).as_str())
            {
                return Err(corrupt(
                    "provider-view history digest differs from its index",
                ));
            }
            for (ordinal, block) in
                history_segment_prefix(connection, &storage.session_id, segment_id, count)?
                    .into_iter()
                    .enumerate()
            {
                stored.push(StoredBlock {
                    section: "history".to_owned(),
                    ordinal: i64::try_from(ordinal)
                        .map_err(|_| corrupt("provider-view history ordinal overflow"))?,
                    block,
                    expires_at_ms,
                });
            }
        } else if request.4.is_some() || request.5.is_some() || request.6.is_some() {
            return Err(corrupt("provider-view history cursor is incomplete"));
        }
        let expected = indexed_blocks(ledger);
        if stored.len() != expected.len()
            || stored.iter().zip(&expected).any(|(stored, expected)| {
                stored.section != expected.section
                    || stored.ordinal != expected.ordinal
                    || &stored.block != expected.block
                    || stored.expires_at_ms != storage.expires_at_ms
            })
        {
            return Err(corrupt(
                "provider-view block index does not match its durable ledger",
            ));
        }
        let mut verified = HashSet::new();
        for block in ledger_block_refs(ledger) {
            if verified.insert(block) {
                self.verify_cas_block(block)?;
            }
        }
        Ok(())
    }

    fn queue_gc(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        blocks: &HashSet<ProviderViewBlockRefV1>,
    ) -> StoreResult<()> {
        let queued_at_ms = to_sqlite_integer(now_ms()?)?;
        let mut insert = transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO provider_view_gc(content_hash, queued_at_ms)
                 VALUES (?1, ?2)",
            )
            .map_err(sqlite_write_error)?;
        for block in blocks {
            insert
                .execute(params![&block.content_hash, queued_at_ms])
                .map_err(sqlite_write_error)?;
        }
        Ok(())
    }

    /// Lazily reads one verified block. Callers reconstructing a request must
    /// drop each returned buffer before asking for the next; this method never
    /// assembles or caches a complete view.
    pub(crate) fn read_block(
        &self,
        connection: &Connection,
        ledger: &ProviderViewLedgerV1,
        block: &ProviderViewBlockRefV1,
    ) -> StoreResult<Vec<u8>> {
        self.verify_membership(connection, ledger, block)?;
        if let Some(bytes) = self
            .hot_blocks
            .lock()
            .map_err(|_| corrupt("provider-view hot-block cache lock is poisoned"))?
            .get(&block.content_hash)
        {
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != block.byte_len {
                return Err(corrupt(
                    "resident provider-view block length does not match its index",
                ));
            }
            return Ok(bytes);
        }
        let artifact = ArtifactRef::new(block.content_hash.clone());
        let bytes = self.cas.get(&artifact).map_err(|error| {
            if error.code == ErrorCode::InvalidArgument {
                corrupt(format!(
                    "indexed provider-view CAS block is missing or invalid: {error}"
                ))
            } else {
                error
            }
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != block.byte_len {
            return Err(corrupt(
                "provider-view CAS block length does not match its index",
            ));
        }
        self.hot_blocks
            .lock()
            .map_err(|_| corrupt("provider-view hot-block cache lock is poisoned"))?
            .insert(block.content_hash.clone(), bytes.clone());
        Ok(bytes)
    }

    pub(crate) fn sweep_expired(
        &self,
        connection: &mut Connection,
        through_ms: u64,
    ) -> StoreResult<usize> {
        let mut schedule = self.sweep_schedule()?;
        let removed = self.sweep_expired_rows(connection, through_ms)?;
        schedule.sweep_completed(Instant::now());
        Ok(removed)
    }

    /// Runs expiry maintenance before a provider-view persist only when its
    /// monotonic time/count watermark is due. A failed sweep stays due so the
    /// next persist retries instead of silently postponing reclamation.
    pub(crate) fn sweep_expired_if_due(
        &self,
        connection: &mut Connection,
        through_ms: u64,
    ) -> StoreResult<Option<usize>> {
        self.sweep_expired_if_due_at(connection, Instant::now(), through_ms)
    }

    fn sweep_expired_if_due_at(
        &self,
        connection: &mut Connection,
        monotonic_now: Instant,
        through_ms: u64,
    ) -> StoreResult<Option<usize>> {
        let mut schedule = self.sweep_schedule()?;
        if !schedule.note_persist_and_is_due(monotonic_now) {
            return Ok(None);
        }
        let removed = self.sweep_expired_rows(connection, through_ms)?;
        schedule.sweep_completed(monotonic_now);
        Ok(Some(removed))
    }

    fn sweep_expired_rows(
        &self,
        connection: &mut Connection,
        through_ms: u64,
    ) -> StoreResult<usize> {
        let through_ms = to_sqlite_integer(through_ms)?;
        let batch_size = i64::try_from(PROVIDER_VIEW_SWEEP_REQUEST_BATCH)
            .map_err(|_| corrupt("provider-view sweep batch exceeds SQLite integer space"))?;
        let mut removed = 0_usize;
        loop {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sqlite_write_error)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO provider_view_gc(content_hash, queued_at_ms)
                     SELECT DISTINCT b.content_hash, ?1
                     FROM provider_view_blocks b
                     JOIN (
                         SELECT session_id, request_ordinal
                         FROM provider_view_requests
                         WHERE expires_at_ms <= ?1
                         ORDER BY expires_at_ms, session_id, request_ordinal
                         LIMIT ?2
                     ) expired
                       ON expired.session_id = b.session_id
                      AND expired.request_ordinal = b.request_ordinal",
                    params![through_ms, batch_size],
                )
                .map_err(sqlite_write_error)?;
            transaction
                .execute(
                    "WITH RECURSIVE expired_segments(id, cutoff) AS (
                         SELECT h.segment_id, h.block_count
                         FROM provider_view_request_history h
                         JOIN provider_view_requests r
                           ON r.session_id = h.session_id
                          AND r.request_ordinal = h.request_ordinal
                         WHERE r.rowid IN (
                             SELECT rowid FROM provider_view_requests
                             WHERE expires_at_ms <= ?1
                             ORDER BY expires_at_ms, session_id, request_ordinal
                             LIMIT ?2
                         )
                         UNION
                         SELECT s.parent_segment_id,
                                MIN(expired_segments.cutoff, s.parent_block_count)
                         FROM expired_segments
                         JOIN provider_view_history_segments s ON s.id = expired_segments.id
                         WHERE s.parent_segment_id IS NOT NULL
                     )
                     INSERT OR IGNORE INTO provider_view_gc(content_hash, queued_at_ms)
                     SELECT DISTINCT b.content_hash, ?1
                     FROM expired_segments x
                     JOIN provider_view_history_segments s ON s.id = x.id
                     JOIN provider_view_history_blocks b
                       ON b.segment_id = s.id
                      AND b.block_ordinal >= s.parent_block_count
                      AND b.block_ordinal < x.cutoff",
                    params![through_ms, batch_size],
                )
                .map_err(sqlite_write_error)?;
            let batch_removed = transaction
                .execute(
                    "DELETE FROM provider_view_requests
                     WHERE rowid IN (
                         SELECT rowid FROM provider_view_requests
                         WHERE expires_at_ms <= ?1
                         ORDER BY expires_at_ms, session_id, request_ordinal
                         LIMIT ?2
                     )",
                    params![through_ms, batch_size],
                )
                .map_err(sqlite_write_error)?;
            transaction.commit().map_err(sqlite_write_error)?;
            removed = removed.saturating_add(batch_removed);
            if batch_removed < PROVIDER_VIEW_SWEEP_REQUEST_BATCH {
                break;
            }
        }
        self.drain_gc(connection)?;
        Ok(removed)
    }

    fn sweep_schedule(&self) -> StoreResult<std::sync::MutexGuard<'_, ProviderViewSweepSchedule>> {
        self.sweep_schedule
            .lock()
            .map_err(|_| corrupt("provider-view sweep schedule lock is poisoned"))
    }

    fn drain_gc(&self, connection: &Connection) -> StoreResult<()> {
        while let Some(hash) = connection
            .query_row(
                "SELECT content_hash FROM provider_view_gc
                 ORDER BY content_hash LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_read_error)?
        {
            let references: i64 = connection
                .query_row(
                    "WITH RECURSIVE live_segments(id, cutoff) AS (
                         SELECT h.segment_id, h.block_count
                         FROM provider_view_request_history h
                         JOIN provider_view_requests r
                           ON r.session_id = h.session_id
                          AND r.request_ordinal = h.request_ordinal
                         UNION
                         SELECT s.parent_segment_id,
                                MIN(live_segments.cutoff, s.parent_block_count)
                         FROM live_segments
                         JOIN provider_view_history_segments s ON s.id = live_segments.id
                         WHERE s.parent_segment_id IS NOT NULL
                     )
                     SELECT
                       (SELECT COUNT(*) FROM provider_view_blocks
                        WHERE content_hash = ?1)
                       +
                       (SELECT COUNT(*) FROM live_segments x
                        JOIN provider_view_history_segments s ON s.id = x.id
                        JOIN provider_view_history_blocks b
                          ON b.segment_id = s.id
                         AND b.block_ordinal >= s.parent_block_count
                         AND b.block_ordinal < x.cutoff
                        WHERE b.content_hash = ?1)",
                    [&hash],
                    |row| row.get(0),
                )
                .map_err(sqlite_read_error)?;
            if references == 0 {
                self.cas.remove(&ArtifactRef::new(hash.clone()))?;
                self.hot_blocks
                    .lock()
                    .map_err(|_| corrupt("provider-view hot-block cache lock is poisoned"))?
                    .remove(&hash);
            }
            connection
                .execute(
                    "DELETE FROM provider_view_gc WHERE content_hash = ?1",
                    [&hash],
                )
                .map_err(sqlite_write_error)?;
        }
        Ok(())
    }

    pub(crate) fn resident_bytes(&self) -> StoreResult<usize> {
        Ok(self
            .hot_blocks
            .lock()
            .map_err(|_| corrupt("provider-view hot-block cache lock is poisoned"))?
            .resident_bytes)
    }

    fn verify_membership(
        &self,
        connection: &Connection,
        ledger: &ProviderViewLedgerV1,
        block: &ProviderViewBlockRefV1,
    ) -> StoreResult<()> {
        let storage = ledger
            .storage
            .as_ref()
            .ok_or_else(|| corrupt("durable provider-view ledger has no disk storage cursor"))?;
        let found = connection
            .query_row(
                "SELECT 1 FROM provider_view_blocks
                 WHERE provider = ?1 AND model = ?2 AND cache_epoch = ?3
                   AND session_id = ?4 AND request_ordinal = ?5
                   AND content_hash = ?6 AND byte_len = ?7 LIMIT 1",
                params![
                    &ledger.provider,
                    &ledger.model,
                    &ledger.cache_epoch,
                    storage.session_id.as_str(),
                    to_sqlite_integer(storage.request_ordinal)?,
                    &block.content_hash,
                    to_sqlite_integer(block.byte_len)?,
                ],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_read_error)?;
        if found.is_some() {
            return Ok(());
        }
        if !ledger.history_blocks.contains(block) {
            return Err(corrupt(
                "provider-view block is not indexed for this request",
            ));
        }
        let history_cursor: Option<(i64, String)> = connection
            .query_row(
                "SELECT h.block_count, h.history_digest
                 FROM provider_view_requests r
                 JOIN provider_view_request_history h
                   ON h.session_id = r.session_id
                  AND h.request_ordinal = r.request_ordinal
                 WHERE r.provider = ?1 AND r.model = ?2 AND r.cache_epoch = ?3
                   AND r.session_id = ?4 AND r.request_ordinal = ?5",
                params![
                    &ledger.provider,
                    &ledger.model,
                    &ledger.cache_epoch,
                    storage.session_id.as_str(),
                    to_sqlite_integer(storage.request_ordinal)?,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_read_error)?;
        match history_cursor {
            Some((count, digest))
                if usize::try_from(count).ok() == Some(ledger.history_blocks.len())
                    && digest == history_digest(&ledger.history_blocks) =>
            {
                Ok(())
            }
            _ => Err(corrupt(
                "provider-view block is not indexed for this request",
            )),
        }
    }

    fn verify_cas_block(&self, block: &ProviderViewBlockRefV1) -> StoreResult<()> {
        let artifact = ArtifactRef::new(block.content_hash.clone());
        let path = self.cas.path_for(&artifact)?;
        let byte_len = fs::metadata(&path)
            .map_err(|error| {
                store_error(
                    ErrorCode::StoreCorrupt,
                    format!(
                        "cannot stat provider-view CAS block {}: {error}",
                        path.display()
                    ),
                    false,
                )
            })?
            .len();
        if byte_len != block.byte_len || !self.cas.verify_artifact(&artifact)? {
            return Err(corrupt(
                "provider-view CAS block is missing, truncated, or corrupt",
            ));
        }
        Ok(())
    }
}

/// `F_BARRIERFSYNC` orders only fsync'd I/O on its own device. The provider
/// view CAS must therefore share a device with `store.sqlite`, which lives
/// directly in `profile_root`. A pre-existing nested mount at the CAS
/// namespace is rejected instead of silently weakening the power-loss
/// referential-integrity guarantee.
#[cfg(target_vendor = "apple")]
fn ensure_provider_view_barrier_device(profile_root: &Path) -> StoreResult<()> {
    use std::os::unix::fs::MetadataExt as _;

    let profile_metadata = fs::metadata(profile_root).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!(
                "cannot inspect provider-view profile device {}: {error}",
                profile_root.display()
            ),
            false,
        )
    })?;
    let cas_root = profile_root.join("provider-view-cas");
    let cas_metadata = fs::metadata(&cas_root).map_err(|error| {
        store_error(
            ErrorCode::Internal,
            format!(
                "cannot inspect provider-view CAS device {}: {error}",
                cas_root.display()
            ),
            false,
        )
    })?;
    if profile_metadata.dev() != cas_metadata.dev() {
        return Err(store_error(
            ErrorCode::InvalidArgument,
            format!(
                "provider-view CAS {} must share a device with profile root {} for ordered persistence",
                cas_root.display(),
                profile_root.display()
            ),
            false,
        ));
    }
    Ok(())
}

struct StoredBlock {
    section: String,
    ordinal: i64,
    block: ProviderViewBlockRefV1,
    expires_at_ms: u64,
}

struct IndexedBlock<'a> {
    section: &'static str,
    ordinal: i64,
    block: &'a ProviderViewBlockRefV1,
}

fn indexed_blocks(ledger: &ProviderViewLedgerV1) -> Vec<IndexedBlock<'_>> {
    let mut blocks = Vec::with_capacity(ledger.history_blocks.len().saturating_add(2));
    blocks.push(IndexedBlock {
        section: "system",
        ordinal: 0,
        block: &ledger.system_block,
    });
    blocks.push(IndexedBlock {
        section: "tools",
        ordinal: 0,
        block: &ledger.tool_schema_block,
    });
    blocks.extend(
        ledger
            .history_blocks
            .iter()
            .enumerate()
            .map(|(ordinal, block)| IndexedBlock {
                section: "history",
                ordinal: i64::try_from(ordinal).unwrap_or(i64::MAX),
                block,
            }),
    );
    blocks
}

fn ledger_block_refs(ledger: &ProviderViewLedgerV1) -> Vec<&ProviderViewBlockRefV1> {
    std::iter::once(&ledger.system_block)
        .chain(std::iter::once(&ledger.tool_schema_block))
        .chain(ledger.history_blocks.iter())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn insert_block(
    transaction: &rusqlite::Transaction<'_>,
    ledger: &ProviderViewLedgerV1,
    session_id: &SessionId,
    request_ordinal: i64,
    section: &str,
    block_ordinal: usize,
    block: &ProviderViewBlockRefV1,
    expires_at_ms: u64,
) -> StoreResult<()> {
    let mut statement = transaction
        .prepare_cached(
            "INSERT INTO provider_view_blocks(
                provider, model, cache_epoch, session_id, request_ordinal,
                section, block_ordinal, content_hash, byte_len, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .map_err(sqlite_write_error)?;
    statement
        .execute(params![
            &ledger.provider,
            &ledger.model,
            &ledger.cache_epoch,
            session_id.as_str(),
            request_ordinal,
            section,
            i64::try_from(block_ordinal).map_err(|_| {
                invalid("provider-view block ordinal exceeds SQLite integer space")
            })?,
            &block.content_hash,
            to_sqlite_integer(block.byte_len)?,
            to_sqlite_integer(expires_at_ms)?,
        ])
        .map_err(sqlite_write_error)?;
    Ok(())
}

struct HotBlockLru {
    cap_bytes: usize,
    resident_bytes: usize,
    order: VecDeque<String>,
    blocks: HashMap<String, Vec<u8>>,
}

impl HotBlockLru {
    fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes,
            resident_bytes: 0,
            order: VecDeque::new(),
            blocks: HashMap::new(),
        }
    }

    fn get(&mut self, hash: &str) -> Option<Vec<u8>> {
        let bytes = self.blocks.get(hash)?.clone();
        self.order.retain(|candidate| candidate != hash);
        self.order.push_back(hash.to_owned());
        Some(bytes)
    }

    fn insert(&mut self, hash: String, bytes: Vec<u8>) {
        self.remove(&hash);
        if bytes.len() > self.cap_bytes {
            return;
        }
        while self.resident_bytes.saturating_add(bytes.len()) > self.cap_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.blocks.remove(&oldest) {
                self.resident_bytes = self.resident_bytes.saturating_sub(removed.len());
            }
        }
        self.resident_bytes = self.resident_bytes.saturating_add(bytes.len());
        self.order.push_back(hash.clone());
        self.blocks.insert(hash, bytes);
    }

    fn remove(&mut self, hash: &str) {
        self.order.retain(|candidate| candidate != hash);
        if let Some(removed) = self.blocks.remove(hash) {
            self.resident_bytes = self.resident_bytes.saturating_sub(removed.len());
        }
    }
}

pub(crate) fn default_expiry_ms() -> StoreResult<u64> {
    Ok(now_ms()?.saturating_add(PROVIDER_VIEW_RETENTION_MS))
}

fn invalid(message: impl Into<String>) -> haider_protocol::error::HaiderError {
    store_error(ErrorCode::InvalidArgument, message, false)
}

fn corrupt(message: impl Into<String>) -> haider_protocol::error::HaiderError {
    store_error(ErrorCode::StoreCorrupt, message, false)
}

fn sqlite_read_error(error: rusqlite::Error) -> haider_protocol::error::HaiderError {
    map_sqlite_error(error)
}

fn sqlite_write_error(error: rusqlite::Error) -> haider_protocol::error::HaiderError {
    map_sqlite_error(error)
}
