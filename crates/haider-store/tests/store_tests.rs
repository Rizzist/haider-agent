use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, SessionId};
use haider_store::{Cas, EventStore, Store};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fmt::Debug;
use std::fs;
use std::sync::{Arc, Barrier};

fn must<T, E: Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("operation failed: {error:?}"),
    }
}

fn test_root() -> tempfile::TempDir {
    must(tempfile::tempdir())
}

fn envelope(session: &SessionId, event_id: &str, payload: Value) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 9_999,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("device-test"),
        authority_epoch: 3,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 9_999,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload,
    }
}

#[test]
fn append_read_and_reopen_replay_are_byte_identical() {
    let root = test_root();
    let session = SessionId::new("session-round-trip");
    let committed_json = {
        let store = must(Store::open(root.path()));
        let mut batch = vec![
            envelope(
                &session,
                "event-1",
                json!({"type": "user_message", "text": "hello"}),
            ),
            envelope(
                &session,
                "event-2",
                json!({"type": "future_payload", "nested": {"number": 42}}),
            ),
        ];

        let range = must(store.append(&mut batch));
        assert_eq!(range.session_id, session);
        assert_eq!((range.first_seq, range.last_seq), (1, 2));
        assert_eq!(range.len(), 2);
        assert_eq!(
            batch.iter().map(|item| item.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(batch.iter().all(|item| item.committed_at_ms != 9_999));

        let read = must(store.read(&session, 0, 10));
        let appended_json = must(serde_json::to_vec(&batch));
        let read_json = must(serde_json::to_vec(&read));
        assert_eq!(read_json, appended_json);
        appended_json
    };

    let reopened = must(Store::open(root.path()));
    let replayed = must(reopened.journal_replay(&session));
    assert_eq!(must(serde_json::to_vec(&replayed)), committed_json);
}

#[test]
fn sequences_are_monotonic_gap_free_and_per_session() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let first_session = SessionId::new("session-a");
    let second_session = SessionId::new("session-b");

    let mut first_batch = vec![
        envelope(&first_session, "a-1", json!({"type": "one"})),
        envelope(&first_session, "a-2", json!({"type": "two"})),
    ];
    let mut other_batch = vec![envelope(
        &second_session,
        "b-1",
        json!({"type": "independent"}),
    )];
    let mut final_batch = vec![envelope(&first_session, "a-3", json!({"type": "three"}))];

    assert_eq!(must(store.append(&mut first_batch)).first_seq, 1);
    assert_eq!(must(store.append(&mut other_batch)).first_seq, 1);
    assert_eq!(must(store.append(&mut final_batch)).first_seq, 3);
    assert_eq!(must(store.latest_seq(&first_session)), 3);
    assert_eq!(must(store.latest_seq(&second_session)), 1);
    assert_eq!(
        must(store.read(&first_session, 1, 1))
            .iter()
            .map(|item| item.seq)
            .collect::<Vec<_>>(),
        [2]
    );
    assert!(must(store.read(&first_session, 0, 0)).is_empty());
}

#[test]
fn concurrent_appends_serialize_on_the_persistent_connection() {
    let root = test_root();
    let store = Arc::new(must(Store::open(root.path())));
    let barrier = Arc::new(Barrier::new(3));
    let session = SessionId::new("session-concurrent");

    let handles = ["concurrent-1", "concurrent-2"]
        .into_iter()
        .map(|event_id| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let session = session.clone();
            std::thread::spawn(move || {
                let mut batch = vec![envelope(&session, event_id, json!({"type": "concurrent"}))];
                barrier.wait();
                let range = must(store.append(&mut batch));
                (range.first_seq, batch.remove(0))
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    let mut committed = handles
        .into_iter()
        .map(|handle| must(handle.join()))
        .collect::<Vec<_>>();
    committed.sort_by_key(|(seq, _)| *seq);

    assert_eq!(
        committed.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        must(store.read(&session, 0, 10))
            .iter()
            .map(|item| item.seq)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn failed_atomic_append_leaves_no_truth_and_does_not_mutate_callers() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-atomic");
    let mut batch = vec![
        envelope(&session, "duplicate-id", json!({"type": "first"})),
        envelope(&session, "duplicate-id", json!({"type": "second"})),
    ];
    let before = batch.clone();

    let Err(error) = store.append(&mut batch) else {
        panic!("duplicate event IDs unexpectedly committed");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(batch, before);
    assert_eq!(must(store.latest_seq(&session)), 0);
    assert!(must(store.read(&session, 0, 10)).is_empty());
}

#[test]
fn cas_put_get_verify_deduplicate_and_detect_corruption() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let bytes = b"durable artifact bytes";
    let artifact = must(store.put(bytes));
    let expected = format!("blake3:{}", blake3::hash(bytes).to_hex());

    assert_eq!(artifact.as_str(), expected);
    assert_eq!(must(store.put(bytes)), artifact);
    assert_eq!(must(store.get(&artifact)), bytes);
    assert!(store.verify(&artifact));

    let object_path = must(store.cas().path_for(&artifact));
    assert_eq!(
        object_path.file_name().and_then(|name| name.to_str()),
        Some(&artifact.as_str()["blake3:".len()..])
    );
    must(fs::write(&object_path, b"corrupt bytes"));
    assert!(!store.verify(&artifact));
    let Err(put_error) = store.put(bytes) else {
        panic!("put unexpectedly accepted a corrupt existing CAS object");
    };
    assert_eq!(put_error.code, ErrorCode::StoreCorrupt);
    let Err(error) = store.get(&artifact) else {
        panic!("corrupt CAS object unexpectedly read successfully");
    };
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
}

#[test]
fn concurrent_cas_puts_of_identical_content_publish_one_verified_object() {
    let root = test_root();
    let store = Arc::new(must(Store::open(root.path())));
    let barrier = Arc::new(Barrier::new(3));
    let bytes = vec![b'x'; 1024 * 1024];

    let handles = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let bytes = bytes.clone();
            std::thread::spawn(move || {
                barrier.wait();
                must(store.put(&bytes))
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    let artifacts = handles
        .into_iter()
        .map(|handle| must(handle.join()))
        .collect::<Vec<_>>();

    assert_eq!(artifacts[0], artifacts[1]);
    assert_eq!(must(store.get(&artifacts[0])), bytes);
    assert!(store.verify(&artifacts[0]));

    let object_path = must(store.cas().path_for(&artifacts[0]));
    let shard = object_path.parent().unwrap_or_else(|| {
        panic!("CAS object path has no shard: {}", object_path.display());
    });
    let entries = must(fs::read_dir(shard))
        .map(|entry| must(entry).path())
        .collect::<Vec<_>>();
    assert_eq!(entries, [object_path]);
}

#[test]
fn malformed_and_traversal_artifact_refs_are_rejected() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let invalid_refs = [
        ArtifactRef::new("blake3:../.."),
        ArtifactRef::new(format!("blake3:../{}", "a".repeat(61))),
        ArtifactRef::new("blake3:abc123"),
        ArtifactRef::new(format!("blake3:{}", "A".repeat(64))),
    ];

    for artifact in invalid_refs {
        let Err(path_error) = store.cas().path_for(&artifact) else {
            panic!("invalid artifact reference resolved to a path: {artifact}");
        };
        assert_eq!(path_error.code, ErrorCode::InvalidArgument);

        let Err(get_error) = store.get(&artifact) else {
            panic!("invalid artifact reference unexpectedly read: {artifact}");
        };
        assert_eq!(get_error.code, ErrorCode::InvalidArgument);
        assert!(!store.verify(&artifact));
    }
}

#[test]
fn profile_lock_is_exclusive_and_released_on_drop() {
    let root = test_root();
    let first = must(Store::open(root.path()));

    let Err(error) = Store::open(root.path()) else {
        panic!("second profile opener unexpectedly acquired the lock");
    };
    assert_eq!(error.code, ErrorCode::StoreLocked);
    assert!(error.retryable);
    drop(first);

    let reopened = must(Store::open(root.path()));
    assert_eq!(reopened.root(), root.path());
}

#[test]
fn migrations_apply_fresh_and_are_idempotent_on_reopen() {
    let root = test_root();
    let database_path = {
        let store = must(Store::open(root.path()));
        assert_eq!(must(store.schema_version()), 1);
        store.database_path().to_path_buf()
    };

    let reopened = must(Store::open(root.path()));
    assert_eq!(must(reopened.schema_version()), 1);
    let connection = must(Connection::open(database_path));
    let registered: u32 = must(connection.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version = 1",
        [],
        |row| row.get(0),
    ));
    assert_eq!(registered, 1);
    for table in ["sessions", "events", "schema_migrations"] {
        let count: u32 = must(connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        ));
        assert_eq!(count, 1, "missing table {table}");
    }
}

#[test]
fn store_refuses_a_database_from_a_newer_writer() {
    let root = test_root();
    let (database_path, supported_version) = {
        let store = must(Store::open(root.path()));
        (
            store.database_path().to_path_buf(),
            must(store.schema_version()),
        )
    };
    let newer_version = supported_version + 1;
    let connection = must(Connection::open(&database_path));
    must(connection.pragma_update(None, "user_version", newer_version));
    drop(connection);

    let Err(error) = Store::open(root.path()) else {
        panic!("store accepted schema version {newer_version}");
    };
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert!(!error.retryable);

    let connection = must(Connection::open(database_path));
    let found: u32 = must(connection.pragma_query_value(None, "user_version", |row| row.get(0)));
    assert_eq!(found, newer_version);
}

#[test]
fn raw_envelope_preserves_unknown_payload_kinds() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-forward-compatible");
    let unknown_payload = json!({
        "type": "quantum_handoff_from_a_newer_writer",
        "new_field": ["kept", {"exactly": true}]
    });
    let mut batch = vec![envelope(
        &session,
        "event-from-the-future",
        unknown_payload.clone(),
    )];

    must(store.append(&mut batch));
    let read = must(store.read(&session, 0, 10));
    assert_eq!(read[0].payload, unknown_payload);
    assert_eq!(
        must(serde_json::to_vec(&read[0])),
        must(serde_json::to_vec(&batch[0]))
    );
}

#[test]
fn dropping_a_connection_mid_transaction_leaves_no_partial_journal() {
    let root = test_root();
    let session = SessionId::new("session-killed-transaction");
    {
        let store = must(Store::open(root.path()));
        let connection = must(Connection::open(store.database_path()));
        must(connection.execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE"));
        must(connection.execute(
            "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
            params![session.as_str(), 1_i64, "{}"],
        ));
        let uncommitted = envelope(&session, "never-committed", json!({"type": "partial"}));
        must(connection.execute(
            "INSERT INTO events(
                session_id, seq, envelope_json, event_id, committed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.as_str(),
                1_i64,
                must(serde_json::to_string(&uncommitted)),
                uncommitted.event_id.as_str(),
                1_i64
            ],
        ));
        drop(connection);
    }

    let reopened = must(Store::open(root.path()));
    assert_eq!(must(reopened.latest_seq(&session)), 0);
    assert!(must(reopened.journal_replay(&session)).is_empty());
}
