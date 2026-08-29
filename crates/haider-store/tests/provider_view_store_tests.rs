#![allow(clippy::expect_used)]

use haider_protocol::cache::{ProviderViewBlobV1, ProviderViewBoundaryV1, ProviderViewLedgerV1};
use haider_protocol::ids::SessionId;
use haider_store::Store;
use rusqlite::{Connection, params};

fn create_session(store: &Store, session_id: &SessionId) {
    let connection = Connection::open(store.database_path()).expect("open test database");
    connection
        .execute(
            "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
            params![session_id.as_str(), 1_i64, "{}"],
        )
        .expect("insert test session");
}

fn provider_view(
    seed: &str,
    payload_len: usize,
) -> (ProviderViewLedgerV1, Vec<ProviderViewBlobV1>) {
    let system = ProviderViewBlobV1::new(format!("system-{seed}").into_bytes());
    let tools = ProviderViewBlobV1::new(format!("tools-{seed}").into_bytes());
    let history = ProviderViewBlobV1::new(vec![seed.as_bytes()[0]; payload_len]);
    let ledger = ProviderViewLedgerV1 {
        provider: "openai".into(),
        model: "gpt-test".into(),
        max_tokens: 4_096,
        dialect: "responses".into(),
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

/// MEMORY ITEM 962: request/session growth may add only disk rows and small
/// hashes. Even actively reading every block cannot grow the resident cache
/// beyond its byte cap, and an oversize block is never admitted.
#[test]
fn provider_view_resident_state_stays_byte_capped_as_requests_grow() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    for session_index in 0..8 {
        let session_id = SessionId::new(format!("provider-view-{session_index}"));
        create_session(&store, &session_id);
        for request_index in 0..16 {
            let seed = format!("{session_index:x}{request_index:x}");
            let (ledger, blobs) = provider_view(&seed, 8 * 1024);
            let stored = store
                .persist_provider_view(&session_id, ledger, blobs)
                .expect("persist provider view");
            for block in std::iter::once(&stored.system_block)
                .chain(std::iter::once(&stored.tool_schema_block))
                .chain(stored.history_blocks.iter())
            {
                let bytes = store
                    .read_provider_view_block(&stored, block)
                    .expect("lazy block read");
                assert_eq!(bytes.len() as u64, block.byte_len);
                drop(bytes);
            }
            assert!(
                store
                    .provider_view_resident_bytes()
                    .expect("resident bytes")
                    <= 64 * 1024,
                "hot-block LRU exceeded its byte cap"
            );
        }
    }

    let session_id = SessionId::new("provider-view-oversize");
    create_session(&store, &session_id);
    let (ledger, blobs) = provider_view("z", 128 * 1024);
    let stored = store
        .persist_provider_view(&session_id, ledger, blobs)
        .expect("persist oversize block");
    let journal_ledger = serde_json::to_vec(&stored).expect("serialize hashes-only ledger");
    assert!(journal_ledger.len() < 2 * 1024);
    assert!(
        !journal_ledger
            .windows(32)
            .any(|window| window == [b'z'; 32])
    );
    let bytes = store
        .read_provider_view_block(&stored, &stored.history_blocks[0])
        .expect("read oversize block");
    drop(bytes);
    assert!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes")
            <= 64 * 1024
    );
}

/// Expiry removes the request cursor and unreferenced bytes, while a shared
/// content-addressed block remains readable for a non-expired request.
#[test]
fn expired_provider_view_sweep_preserves_live_shared_blocks() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-expiry");
    create_session(&store, &session_id);

    let (expired_ledger, expired_blobs) = provider_view("s", 4 * 1024);
    let expired = store
        .persist_provider_view_until(&session_id, expired_ledger, expired_blobs, 10)
        .expect("persist expired view");
    let (live_ledger, live_blobs) = provider_view("s", 4 * 1024);
    let live = store
        .persist_provider_view_until(&session_id, live_ledger, live_blobs, 20)
        .expect("persist live view");

    assert_eq!(
        store
            .sweep_expired_provider_views(10)
            .expect("sweep expired views"),
        1
    );
    assert!(store.verify_provider_view(&expired).is_err());
    store
        .verify_provider_view(&live)
        .expect("live view verifies");
    assert_eq!(
        store
            .read_provider_view_block(&live, &live.history_blocks[0])
            .expect("shared live block"),
        vec![b's'; 4 * 1024]
    );

    assert_eq!(
        store
            .sweep_expired_provider_views(20)
            .expect("sweep final view"),
        1
    );
    assert!(store.verify_provider_view(&live).is_err());
    assert_eq!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes"),
        0
    );

    let (next_ledger, next_blobs) = provider_view("n", 256);
    let next = store
        .persist_provider_view_until(&session_id, next_ledger, next_blobs, 30)
        .expect("persist after sweep");
    assert!(
        next.storage
            .as_ref()
            .expect("next storage cursor")
            .request_ordinal
            > live
                .storage
                .as_ref()
                .expect("live storage cursor")
                .request_ordinal
    );
}

/// Cleanup works in multiple fixed-size SQL batches without admitting the
/// expired bytes into the resident hot-block cache.
#[test]
fn expired_provider_view_sweep_handles_more_than_one_batch() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-batched-expiry");
    create_session(&store, &session_id);

    for index in 0..257 {
        let (ledger, blobs) = provider_view(&format!("expired-{index}"), 64);
        store
            .persist_provider_view_until(&session_id, ledger, blobs, 10)
            .expect("persist expired provider view");
    }
    assert_eq!(
        store
            .sweep_expired_provider_views(10)
            .expect("sweep all expiry batches"),
        257
    );
    assert_eq!(
        store
            .provider_view_resident_bytes()
            .expect("resident bytes"),
        0
    );

    let connection = Connection::open(store.database_path()).expect("open test database");
    let request_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_view_requests", [], |row| {
            row.get(0)
        })
        .expect("count provider-view requests");
    let gc_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_view_gc", [], |row| {
            row.get(0)
        })
        .expect("count provider-view GC queue");
    assert_eq!(request_count, 0);
    assert_eq!(gc_count, 0);
}

/// Request indexes intentionally outlive a deleted source session for fork
/// replay, so recreating an opaque ID must resume above retained ordinals.
#[test]
fn provider_view_request_ordinal_survives_session_id_reuse() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("provider-view-reused-session");
    create_session(&store, &session_id);

    let (first_ledger, first_blobs) = provider_view("first", 64);
    let first = store
        .persist_provider_view_until(&session_id, first_ledger, first_blobs, u64::MAX / 2)
        .expect("persist first provider view");
    store
        .delete_session(&session_id)
        .expect("delete source session");
    let connection = Connection::open(store.database_path()).expect("open test database");
    let cursor_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_view_session_cursors WHERE session_id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .expect("count deleted session cursors");
    assert_eq!(cursor_count, 0);
    drop(connection);
    create_session(&store, &session_id);

    let (second_ledger, second_blobs) = provider_view("second", 64);
    let second = store
        .persist_provider_view_until(&session_id, second_ledger, second_blobs, u64::MAX / 2)
        .expect("persist provider view after ID reuse");
    assert!(
        second
            .storage
            .as_ref()
            .expect("second storage cursor")
            .request_ordinal
            > first
                .storage
                .as_ref()
                .expect("first storage cursor")
                .request_ordinal
    );
    store
        .verify_provider_view(&first)
        .expect("retained source provider view");
    store
        .verify_provider_view(&second)
        .expect("reused-session provider view");
}
