#![allow(clippy::expect_used)]

use super::*;
use crate::Store;
use haider_protocol::cache::{ProviderViewBlobV1, ProviderViewBoundaryV1};
use rusqlite::params;

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
        dialect: "test-dialect".into(),
        serialization_version: "haider.provider-view.json.v1".into(),
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
