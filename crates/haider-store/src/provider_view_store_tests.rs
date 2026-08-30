#![allow(clippy::expect_used)]

use super::*;
use crate::Store;
use haider_protocol::cache::{ProviderViewBlobV1, ProviderViewBoundaryV1};
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

    for _ in 1..PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL {
        let (ledger, blobs) = provider_view("warmup");
        store
            .persist_provider_view(&session_id, ledger, blobs)
            .expect("warm sweep counter");
    }

    for _ in 0..PROVIDER_VIEW_SWEEP_PERSIST_INTERVAL {
        let (expired_ledger, expired_blobs) = provider_view("expired-sentinel");
        store
            .persist_provider_view_until(&session_id, expired_ledger, expired_blobs, 0)
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
