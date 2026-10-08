#![allow(clippy::expect_used)]

use super::*;
use crate::Store;
use haider_protocol::cache::{
    ProviderViewBlobSegmentV1, ProviderViewBlobV1, ProviderViewBoundaryV1,
};
use haider_protocol::reply::ReplyArenaWriter;
use rusqlite::params;
use std::sync::{Arc, Mutex};

fn create_session(store: &Store, session_id: &SessionId) {
    let connection = Connection::open(store.database_path()).expect("open test database");
    connection
        .execute(
            "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
            params![session_id.as_str(), 1_i64, "{}"],
        )
        .expect("insert test session");
}

fn provider_view(seed: &str) -> (ProviderViewLedgerV1, Vec<ProviderViewBlobV1>) {
    let system = ProviderViewBlobV1::new(format!("system-{seed}").into_bytes());
    let tools = ProviderViewBlobV1::new(format!("tools-{seed}").into_bytes());
    let history = ProviderViewBlobV1::new(format!("history-{seed}").into_bytes());
    let ledger = ProviderViewLedgerV1 {
        provider: "test-provider".into(),
        model: "test-model".into(),
        max_tokens: 4_096,
        dialect: "test-dialect".into(),
        serialization_version: "haider.provider-view.json.v2".into(),
        header_epoch: "header".into(),
        cache_epoch: "cache".into(),
        compaction_epoch: "root".into(),
        reasoning_retention: "append_only_provider_opaque_v1:test".into(),
        account_scope: Some("account".into()),
        stable_history_end: 1,
        current_user_start: 1,
        latest_compaction_summary_end: None,
        trim_sentinel: "root".into(),
        boundaries: vec![ProviderViewBoundaryV1 {
            section: "history".into(),
            message_end: Some(1),
        }],
        system_block: system.block.clone(),
        tool_schema_block: tools.block.clone(),
        history_blocks: vec![history.block.clone()],
        storage: None,
    };
    (ledger, vec![system, tools, history])
}

#[test]
fn segmented_reply_blob_hashes_persists_and_reads_exact_legacy_json() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-segmented-reply");
    create_session(&store, &session_id);
    let mut arena = ReplyArenaWriter::new()
        .with_incremental_json_view(br#"{"content":"#, br#","role":"assistant"}"#);
    let _ = arena.append("left \"quote\"\n".to_owned());
    let _ = arena.append("مرز 😀 right".to_owned());
    let reply = arena.seal();
    let history = ProviderViewBlobV1::segmented(vec![
        ProviderViewBlobSegmentV1::Bytes(br#"{"content":"#.to_vec()),
        ProviderViewBlobSegmentV1::JsonString(reply),
        ProviderViewBlobSegmentV1::Bytes(br#","role":"assistant"}"#.to_vec()),
    ])
    .expect("segmented provider-view blob");
    assert!(history.is_segmented());
    assert!(history.is_incrementally_hashed());
    let expected = serde_json::to_vec(&serde_json::json!({
        "role": "assistant",
        "content": "left \"quote\"\nمرز 😀 right",
    }))
    .expect("legacy JSON");
    assert_eq!(
        history.computed_block().expect("computed block"),
        history.block
    );
    assert_eq!(history.byte_len(), expected.len());

    let system = ProviderViewBlobV1::new(b"null".to_vec());
    let tools = ProviderViewBlobV1::new(b"null".to_vec());
    let ledger = ProviderViewLedgerV1 {
        provider: "test-provider".into(),
        model: "test-model".into(),
        max_tokens: 4_096,
        dialect: "test-dialect".into(),
        serialization_version: "haider.provider-view.json.v2".into(),
        header_epoch: "header".into(),
        cache_epoch: "cache".into(),
        compaction_epoch: "root".into(),
        reasoning_retention: "append_only_provider_opaque_v1:test".into(),
        account_scope: None,
        stable_history_end: 1,
        current_user_start: 1,
        latest_compaction_summary_end: None,
        trim_sentinel: "root".into(),
        boundaries: Vec::new(),
        system_block: system.block.clone(),
        tool_schema_block: tools.block.clone(),
        history_blocks: vec![history.block.clone()],
        storage: None,
    };
    let history_ref = history.block.clone();
    let stored = store
        .persist_provider_view(&session_id, ledger, vec![system, tools, history])
        .expect("persist segmented view");
    assert_eq!(
        store
            .read_provider_view_block(&stored, &history_ref)
            .expect("read segmented block"),
        expected
    );
}

/// MUTATION CHECK: restore per-blob full syncs, omit/downgrade the trailing
/// barrier, or move it below the SQLite transaction. Expected runtime failure:
/// the count differs from one or the callback observes an indexed reference.
#[test]
fn provider_view_persist_uses_one_trailing_barrier_before_indexing() {
    for unique_blob_count in [1_usize, 3] {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let session_id = SessionId::new(format!("provider-view-sync-{unique_blob_count}"));
        create_session(&store, &session_id);
        let (mut ledger, mut blobs) = provider_view(&format!("sync-{unique_blob_count}"));
        if unique_blob_count == 1 {
            let block = blobs[0].block.clone();
            ledger.system_block = block.clone();
            ledger.tool_schema_block = block.clone();
            ledger.history_blocks = vec![block];
            blobs.truncate(1);
        }

        let observations = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&observations);
        let database_path = store.database_path().to_path_buf();
        let persisted = crate::cas::with_cas_sync_test_hook(
            move |_path, policy, target| {
                observed
                    .lock()
                    .expect("CAS sync observation lock")
                    .push((policy, target));
                if policy == haider_platform::SyncPolicy::Barrier {
                    let connection =
                        Connection::open(&database_path).expect("observe provider-view index");
                    connection
                        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
                        .expect("trailing barrier precedes the index write transaction");
                    let indexed: i64 = connection
                        .query_row("SELECT COUNT(*) FROM provider_view_requests", [], |row| {
                            row.get(0)
                        })
                        .expect("count provider-view index rows");
                    assert_eq!(indexed, 0, "blob durability precedes its index reference");
                }
            },
            || {
                store
                    .persist_provider_view(&session_id, ledger, blobs)
                    .expect("persist provider view")
            },
        );
        assert!(persisted.storage.is_some());

        let observations = observations.lock().expect("CAS sync observations");
        assert_eq!(
            observations
                .iter()
                .filter(|(policy, _)| *policy == haider_platform::SyncPolicy::Barrier)
                .count(),
            1,
            "one barrier closes every persist regardless of blob count"
        );
        assert_eq!(
            observations
                .iter()
                .filter(|(policy, _)| *policy == haider_platform::SyncPolicy::Full)
                .count(),
            0,
            "provider-view persistence no longer drains volatile device caches"
        );
        assert_eq!(
            observations
                .iter()
                .filter(|(policy, target)| {
                    *policy == haider_platform::SyncPolicy::Plain
                        && *target == crate::cas::CasSyncTarget::File
                })
                .count(),
            unique_blob_count,
            "each new blob receives one plain file sync"
        );
        assert_eq!(
            observations
                .iter()
                .filter(|(policy, target)| {
                    *policy == haider_platform::SyncPolicy::Plain
                        && *target == crate::cas::CasSyncTarget::Directory
                })
                .count(),
            unique_blob_count,
            "each new blob publishes through one plain directory sync"
        );
    }
}

/// MUTATION CHECK: make the persist counter permanently due or forget to
/// reset it after a sweep. Either mutation makes one N-persist window report
/// more than one sweep. Forgetting the count deadline makes it report zero.
#[test]
fn consecutive_persists_schedule_at_most_one_sweep_per_count_window() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-count-window");
    create_session(&store, &session_id);
    // Expiry is clamped monotone per session, so the already-expired
    // sentinels live in their own session.
    let sentinel_session = SessionId::new("provider-view-count-window-sentinels");
    create_session(&store, &sentinel_session);

    for _ in 1..PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL {
        let (ledger, blobs) = provider_view("warmup");
        store
            .persist_provider_view(&session_id, ledger, blobs)
            .expect("warm sweep counter");
    }

    for _ in 0..PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL {
        let (expired_ledger, expired_blobs) = provider_view("expired-sentinel");
        store
            .persist_provider_view_until(&sentinel_session, expired_ledger, expired_blobs, 0)
            .expect("persist expired sweep sentinel");
        let (ledger, blobs) = provider_view("counted-persist");
        store
            .persist_provider_view(&session_id, ledger, blobs)
            .expect("production provider-view persist");
    }
    let connection = Connection::open(store.database_path()).expect("database");
    let remaining_expired: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_view_requests WHERE expires_at_ms = 0",
            [],
            |row| row.get(0),
        )
        .expect("count expired sweep sentinels");
    assert_eq!(
        remaining_expired,
        i64::try_from(PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL - 1)
            .expect("sweep interval fits SQLite count"),
        "N consecutive production persists must run exactly one sweep"
    );
}

/// MUTATION CHECK: changing the seven-day policy or disconnecting the default
/// expiry helper from it fails this literal retention-duration pin.
#[test]
fn default_expiry_preserves_the_seven_day_retention_policy() {
    const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
    assert_eq!(PROVIDER_VIEW_RETENTION_MS, SEVEN_DAYS_MS);
    let before = now_ms().expect("time before default expiry");
    let expiry = default_expiry_ms().expect("default expiry");
    let after = now_ms().expect("time after default expiry");
    assert!(expiry >= before.saturating_add(SEVEN_DAYS_MS));
    assert!(expiry <= after.saturating_add(SEVEN_DAYS_MS));
}

/// MUTATION CHECK: change expiry from `<=` to `<`, remove the time watermark,
/// or fail to re-arm it. The exact retention-boundary removal or second due
/// result fails while the just-before-boundary verification pins retention.
#[test]
fn due_sweep_expires_at_the_retention_boundary_and_rearms() {
    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("provider-view-due-sweep");
    let store = Store::open(root.path()).expect("store");
    create_session(&store, &session_id);
    let (ledger, blobs) = provider_view("retention");
    let stored = store
        .persist_provider_view_until(&session_id, ledger, blobs, PROVIDER_VIEW_RETENTION_MS)
        .expect("persist provider view at retention boundary");
    drop(store);

    let provider_views = ProviderViewStore::open(root.path()).expect("provider-view store");
    let mut connection = Connection::open(root.path().join("store.sqlite")).expect("database");
    let opened_at = Instant::now();
    let first_due = opened_at + PROVIDER_VIEW_SWEEP_INTERVAL;
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, first_due, PROVIDER_VIEW_RETENTION_MS - 1,)
            .expect("pre-expiry due sweep"),
        Some(0)
    );
    provider_views
        .verify(&connection, &stored)
        .expect("view remains live before exact retention boundary");

    let second_due = first_due + PROVIDER_VIEW_SWEEP_INTERVAL;
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, second_due, PROVIDER_VIEW_RETENTION_MS,)
            .expect("expiry due sweep"),
        Some(1)
    );
    assert!(
        provider_views.verify(&connection, &stored).is_err(),
        "exact retention boundary must expire the durable cursor"
    );
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, second_due, PROVIDER_VIEW_RETENTION_MS,)
            .expect("immediate follow-up persist"),
        None,
        "a completed due sweep must not run again on the next persist"
    );
}

/// Seeds `sessions` unrelated live sessions directly in SQL. Each has one
/// trunk of `history` blocks and `requests` leaves with one own block each,
/// which is the shape a long-running session produces.
fn seed_live_sessions(connection: &Connection, sessions: usize, requests: usize, history: usize) {
    let far = 4_000_000_000_000_i64;
    let mut unique = 0_u64;
    let mut next_hash = || {
        unique += 1;
        format!("blake3:{unique:064x}")
    };
    let transaction = connection
        .unchecked_transaction()
        .expect("seed transaction");
    for session in 0..sessions {
        let session_id = format!("seeded-live-{session}");
        transaction
            .execute(
                "INSERT INTO provider_view_history_segments(session_id) VALUES (?1)",
                [&session_id],
            )
            .expect("trunk");
        let trunk = transaction.last_insert_rowid();
        for ordinal in 0..history {
            transaction
                .execute(
                    "INSERT INTO provider_view_history_blocks(
                        segment_id, block_ordinal, content_hash, byte_len
                     ) VALUES (?1, ?2, ?3, 1)",
                    params![trunk, ordinal as i64, next_hash()],
                )
                .expect("trunk block");
        }
        for request in 1..=requests {
            transaction
                .execute(
                    "INSERT INTO provider_view_requests(
                        session_id, request_ordinal, provider, model, cache_epoch, expires_at_ms
                     ) VALUES (?1, ?2, 'p', 'm', 'c', ?3)",
                    params![&session_id, request as i64, far],
                )
                .expect("request");
            transaction
                .execute(
                    "INSERT INTO provider_view_history_segments(
                        session_id, parent_segment_id, parent_block_count
                     ) VALUES (?1, ?2, ?3)",
                    params![&session_id, trunk, history as i64],
                )
                .expect("leaf");
            let leaf = transaction.last_insert_rowid();
            transaction
                .execute(
                    "INSERT INTO provider_view_history_blocks(
                        segment_id, block_ordinal, content_hash, byte_len
                     ) VALUES (?1, ?2, ?3, 1)",
                    params![leaf, history as i64, next_hash()],
                )
                .expect("leaf block");
            transaction
                .execute(
                    "INSERT INTO provider_view_request_history(
                        session_id, request_ordinal, segment_id, block_count, history_digest
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        &session_id,
                        request as i64,
                        leaf,
                        (history + 1) as i64,
                        format!("{request:064x}")
                    ],
                )
                .expect("request history");
        }
    }
    transaction.commit().expect("seed commit");
}

/// SQLite VM operations (in units of 64) one full expiry sweep of a fixed
/// expired session costs next to `unrelated` large live sessions.
fn expiry_sweep_ops(unrelated: usize) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("provider-view-expiring-session");
    {
        let store = Store::open(root.path()).expect("store");
        create_session(&store, &session_id);
        let mut texts = Vec::new();
        for turn in 0..12 {
            texts.push(format!("expiring-turn-{turn}"));
            let history = texts
                .iter()
                .map(|text| ProviderViewBlobV1::new(text.clone().into_bytes()))
                .collect::<Vec<_>>();
            let (mut ledger, mut blobs) = provider_view("expiring");
            ledger.history_blocks = history.iter().map(|blob| blob.block.clone()).collect();
            blobs.truncate(2);
            blobs.extend(history);
            store
                .persist_provider_view_until(&session_id, ledger, blobs, 10)
                .expect("persist expiring view");
        }
    }
    let provider_views = ProviderViewStore::open(root.path()).expect("provider-view store");
    let mut connection = Connection::open(root.path().join("store.sqlite")).expect("database");
    seed_live_sessions(&connection, unrelated, 40, 200);
    let ops = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&ops);
    connection
        .progress_handler(
            64,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .expect("install progress handler");
    assert_eq!(
        provider_views
            .sweep_expired(&mut connection, 10)
            .expect("sweep expiring session"),
        12
    );
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .expect("remove progress handler");
    let remaining: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_view_requests WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .expect("remaining expiring requests");
    assert_eq!(remaining, 0);
    ops.load(Ordering::Relaxed)
}

/// Stage 4 defect 3: GC re-derived every live request's reachable history for
/// each queued hash, so an expiry sweep cost O(queued x live x history) and
/// could run inline before a persist.
///
/// MUTATION CHECK: compute liveness over every live request (the former
/// `drain_gc` recursion) instead of the sessions owning the hash. Expected
/// runtime failure: doubling the unrelated live store doubles the cost.
#[test]
fn expiry_sweep_cost_does_not_scale_with_unrelated_live_history() {
    let small = expiry_sweep_ops(20);
    let large = expiry_sweep_ops(40);
    assert!(
        large * 4 <= small * 5,
        "sweep cost must not grow with unrelated live sessions: {small} -> {large}"
    );
}

/// MUTATION CHECK: loop `maintenance_step` inside the due path or reset the
/// schedule before the backlog is drained. Expected runtime failure: one
/// persist-time call expires the whole backlog, or the remainder is postponed
/// for a full sweep interval.
#[test]
fn due_sweep_runs_one_bounded_step_and_stays_due_until_drained() {
    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("provider-view-bounded-due-sweep");
    let backlog = PROVIDER_VIEW_SWEEP_REQUEST_BATCH + 40;
    {
        let store = Store::open(root.path()).expect("store");
        create_session(&store, &session_id);
        for index in 0..backlog {
            let (ledger, blobs) = provider_view(&format!("backlog-{index}"));
            store
                .persist_provider_view_until(&session_id, ledger, blobs, 0)
                .expect("persist expired backlog");
        }
    }
    let provider_views = ProviderViewStore::open(root.path()).expect("provider-view store");
    let mut connection = Connection::open(root.path().join("store.sqlite")).expect("database");
    let due = Instant::now() + PROVIDER_VIEW_SWEEP_INTERVAL;
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, due, 0)
            .expect("first bounded step"),
        Some(PROVIDER_VIEW_SWEEP_REQUEST_BATCH)
    );
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, due, 0)
            .expect("continued step"),
        Some(40)
    );
    assert_eq!(
        provider_views
            .sweep_expired_if_due_at(&mut connection, due, 0)
            .expect("drained"),
        None
    );
    let (requests, queued): (i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM provider_view_requests),
                    (SELECT COUNT(*) FROM provider_view_gc)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("counts");
    assert_eq!((requests, queued), (0, 0));
    for index in [0, backlog - 1] {
        let (ledger, _) = provider_view(&format!("backlog-{index}"));
        let path = provider_views
            .cas
            .path_for(&ArtifactRef::new(
                ledger.history_blocks[0].content_hash.clone(),
            ))
            .expect("CAS path");
        assert!(!path.exists(), "expired unique block is reclaimed");
    }
}
