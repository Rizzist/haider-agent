#![allow(clippy::expect_used)]

use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION, envelope_weight_bytes,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{ArtifactRef, DeviceId, EventId, SessionId};
use haider_store::{Cas, EventStore, SessionProjectionCheckpoint, Store};
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

fn must_err<T: Debug, E>(result: Result<T, E>) -> E {
    match result {
        Ok(value) => panic!("operation unexpectedly succeeded: {value:?}"),
        Err(error) => error,
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

/// MUTATION CHECK: make the reader accept only one SQLite storage class.
/// Expected failure: either the hand-inserted legacy row or the API-appended
/// MessagePack row cannot be replayed across the format boundary.
#[test]
fn mixed_legacy_json_and_msgpack_rows_obey_journal_laws() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-mixed-encoding");
    let mut legacy = envelope(&session, "legacy-json", json!({"type": "legacy_fact"}));
    legacy.seq = 1;
    legacy.committed_at_ms = 101;

    let connection = must(Connection::open(store.database_path()));
    must(connection.execute(
        "INSERT INTO sessions(id, created_at_ms, meta_json) VALUES (?1, ?2, ?3)",
        params![session.as_str(), 101_i64, "{}"],
    ));
    must(connection.execute(
        "INSERT INTO events(
            session_id, seq, envelope_json, event_id, committed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            session.as_str(),
            1_i64,
            must(serde_json::to_string(&legacy)),
            legacy.event_id.as_str(),
            101_i64,
        ],
    ));
    drop(connection);

    let mut current = vec![envelope(
        &session,
        "current-msgpack",
        json!({"type": "current_fact"}),
    )];
    let range = must(store.append(&mut current));
    assert_eq!((range.first_seq, range.last_seq), (2, 2));
    assert_eq!(must(store.latest_seq(&session)), 2);
    assert_eq!(
        must(store.read(&session, 0, 10)),
        [legacy.clone(), current[0].clone()]
    );
    assert_eq!(must(store.read(&session, 1, 10)), current);
    assert_eq!(
        must(store.journal_replay(&session)),
        [legacy, current[0].clone()]
    );
}

/// MUTATION CHECK: bind encoded bytes as text or omit the payload-kind index.
/// Expected failure: SQLite reports the wrong storage class or kind.
#[test]
fn append_stores_msgpack_blob_and_payload_kind() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-blob-storage-pin");
    let mut batch = vec![envelope(
        &session,
        "blob-storage-pin",
        json!({"type": "storage_pin", "value": 42}),
    )];
    must(store.append(&mut batch));

    let connection = must(Connection::open(store.database_path()));
    let (storage_class, kind): (String, String) = must(connection.query_row(
        "SELECT typeof(envelope_json), payload_kind FROM events WHERE event_id = ?1",
        [batch[0].event_id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ));
    assert_eq!(storage_class, "blob");
    assert_eq!(kind, "storage_pin");
}

/// MUTATION CHECK: insert a one-line `UPDATE events SET envelope_json = X''`
/// into the checkpoint transaction. Expected runtime failure: the raw journal
/// blob comparison changes even though the checkpoint write reports success.
#[test]
fn projection_checkpoint_write_leaves_journal_bytes_unchanged() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("checkpoint-journal-immutability");
    let mut batch = vec![envelope(
        &session,
        "checkpoint-boundary-event",
        json!({"type": "checkpoint_boundary_fixture"}),
    )];
    must(store.append(&mut batch));
    let connection = must(Connection::open(store.database_path()));
    let before: Vec<Vec<u8>> = must(
        connection
            .prepare("SELECT envelope_json FROM events WHERE session_id = ?1 ORDER BY seq")
            .and_then(|mut statement| {
                statement
                    .query_map([session.as_str()], |row| row.get(0))?
                    .collect()
            }),
    );

    must(
        store.put_session_projection_checkpoint(&SessionProjectionCheckpoint {
            session_id: session.clone(),
            projection: "prompt_history".into(),
            timeline_key: "main-agentless".into(),
            through_seq: batch[0].seq,
            boundary_event_id: batch[0].event_id.clone(),
            payload: br#"{"shape_version":1}"#.to_vec(),
        }),
    );

    let after: Vec<Vec<u8>> = must(
        connection
            .prepare("SELECT envelope_json FROM events WHERE session_id = ?1 ORDER BY seq")
            .and_then(|mut statement| {
                statement
                    .query_map([session.as_str()], |row| row.get(0))?
                    .collect()
            }),
    );
    assert_eq!(after, before);
    assert_eq!(must(store.latest_seq(&session)), 1);
}

/// MUTATION CHECK: remove `timeline_key = ?3` from the checkpoint lookup.
/// Expected runtime failure: the branch-B cache miss returns branch A's row.
#[test]
fn projection_checkpoint_lookup_is_timeline_exact_and_corruption_is_reported() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("checkpoint-timeline-keying");
    let mut batch = vec![envelope(
        &session,
        "timeline-a-boundary",
        json!({"type": "checkpoint_boundary_fixture"}),
    )];
    must(store.append(&mut batch));
    let checkpoint = SessionProjectionCheckpoint {
        session_id: session.clone(),
        projection: "prompt_history".into(),
        timeline_key: "branch-a".into(),
        through_seq: batch[0].seq,
        boundary_event_id: batch[0].event_id.clone(),
        payload: br#"{"shape_version":1}"#.to_vec(),
    };
    must(store.put_session_projection_checkpoint(&checkpoint));
    assert_eq!(
        must(store.session_projection_checkpoint(&session, "prompt_history", "branch-a")),
        Some(checkpoint)
    );
    assert_eq!(
        must(store.session_projection_checkpoint(&session, "prompt_history", "branch-b")),
        None
    );

    let connection = must(Connection::open(store.database_path()));
    must(connection.execute(
        "UPDATE session_projection_checkpoints SET payload = X'00' WHERE session_id = ?1",
        [session.as_str()],
    ));
    let error = match store.session_projection_checkpoint(&session, "prompt_history", "branch-a") {
        Err(error) => error,
        Ok(checkpoint) => {
            panic!("digest-invalid checkpoint must be reported, got {checkpoint:?}")
        }
    };
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert!(!error.retryable);
    assert!(
        error.message.contains("prompt_history") && error.message.contains("branch-a"),
        "corruption evidence must identify the disabled checkpoint: {error:?}"
    );

    must(connection.execute(
        "UPDATE session_projection_checkpoints SET payload = 7 WHERE session_id = ?1",
        [session.as_str()],
    ));
    let error = match store.session_projection_checkpoint(&session, "prompt_history", "branch-a") {
        Err(error) => error,
        Ok(checkpoint) => {
            panic!("storage-class-invalid checkpoint must be reported, got {checkpoint:?}")
        }
    };
    assert_eq!(error.code, ErrorCode::StoreCorrupt);
    assert!(!error.retryable);
}

/// MUTATION CHECK: remove the global UNIQUE law on `events.event_id`.
/// Expected failure: the duplicate blob commits and advances the journal head.
#[test]
fn duplicate_event_id_is_rejected_after_blob_commit() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-blob-duplicate");
    let mut original = vec![envelope(
        &session,
        "blob-duplicate-id",
        json!({"type": "original"}),
    )];
    must(store.append(&mut original));

    let mut duplicate = vec![envelope(
        &session,
        "blob-duplicate-id",
        json!({"type": "duplicate"}),
    )];
    let Err(error) = store.append(&mut duplicate) else {
        panic!("duplicate event ID unexpectedly committed");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(must(store.latest_seq(&session)), 1);
    assert_eq!(must(store.read(&session, 0, 10)), original);
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
    must(std::fs::write(
        root.path().join("lock"),
        b"pid=1\ncreated_at_ms=1\n",
    ));
    let first = must(Store::open(root.path()));
    let owner = must(std::fs::read_to_string(root.path().join("lock.owner")));
    assert!(owner.contains(&format!("pid={}", std::process::id())));
    assert!(owner.lines().any(|line| line.starts_with("created_at_ms=")));

    let Err(error) = Store::open(root.path()) else {
        panic!("second profile opener unexpectedly acquired the lock");
    };
    assert_eq!(error.code, ErrorCode::StoreLocked);
    assert!(error.retryable);
    assert_eq!(
        must(std::fs::read_to_string(root.path().join("lock.owner"))),
        owner,
        "the losing opener must not replace incumbent diagnostics"
    );
    drop(first);
    assert!(!root.path().join("lock.owner").exists());
    assert_eq!(must(std::fs::read(root.path().join("lock"))), b"");

    let reopened = must(Store::open(root.path()));
    assert_eq!(reopened.root(), root.path());
    assert!(root.path().join("lock.owner").is_file());
    drop(reopened);
    assert_eq!(must(std::fs::read(root.path().join("lock"))), b"");
    assert!(!root.path().join("lock.owner").exists());
}

#[test]
fn worker_generation_strictly_increases_across_sequential_opens() {
    let root = test_root();
    let first_generation = {
        let store = must(Store::open(root.path()));
        store.worker_generation()
    };
    let second_generation = {
        let store = must(Store::open(root.path()));
        store.worker_generation()
    };

    assert!(first_generation > 0);
    assert!(
        second_generation > first_generation,
        "profile generation did not advance: {first_generation} -> {second_generation}"
    );
}

#[test]
fn profile_lease_excludes_before_store_open_and_releases_if_unused() {
    let root = test_root();
    let lease = must(Store::acquire_profile(root.path()));
    let Err(error) = Store::acquire_profile(root.path()) else {
        panic!("second lease unexpectedly acquired the profile lock");
    };
    assert_eq!(error.code, ErrorCode::StoreLocked);
    drop(lease);

    let reopened = must(Store::acquire_profile(root.path()));
    let store = must(Store::open_locked(reopened));
    assert_eq!(store.root(), root.path());
}

#[test]
fn daemon_generation_is_distinct_explicit_and_durable() {
    let root = test_root();
    let first_worker = {
        let store = must(Store::open(root.path()));
        let first_daemon = must(store.advance_daemon_generation());
        let second_daemon = must(store.advance_daemon_generation());
        assert_eq!(second_daemon, first_daemon + 1);
        must(store.flush());
        store.worker_generation()
    };
    let store = must(Store::open(root.path()));
    assert_eq!(store.worker_generation(), first_worker + 1);
    assert_eq!(must(store.advance_daemon_generation()), 3);
}

#[test]
fn migrations_apply_fresh_and_are_idempotent_on_reopen() {
    let root = test_root();
    let database_path = {
        let store = must(Store::open(root.path()));
        assert_eq!(must(store.schema_version()), 26);
        store.database_path().to_path_buf()
    };

    let reopened = must(Store::open(root.path()));
    assert_eq!(must(reopened.schema_version()), 26);
    let connection = must(Connection::open(database_path));
    let registered: u32 = must(connection.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version BETWEEN 1 AND 14",
        [],
        |row| row.get(0),
    ));
    assert_eq!(registered, 14);
    for table in [
        "sessions",
        "events",
        "schema_migrations",
        "profile_meta",
        "menu_resolutions",
        "command_receipts",
        "account_alias_reservations",
        "provider_models",
        "delegations",
        "branches",
        "hook_dispatch_outbox",
        "session_projection_checkpoints",
        "run_head_sessions",
        "run_heads",
        "loom_cli_install_jobs",
        "loom_cli_install_items",
        "loom_cli_install_events",
        "loom_agent_type_revisions",
        "loom_workflow_revisions",
        "provider_view_session_cursors",
        "provider_view_requests",
        "provider_view_blocks",
        "provider_view_gc",
        "workflow_graph_instances",
        "workflow_node_states",
    ] {
        let count: u32 = must(connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        ));
        assert_eq!(count, 1, "missing table {table}");
    }
}

#[test]
fn typed_agent_install_job_schema_is_durable_and_bounded() {
    let root = test_root();
    let database_path = {
        let store = must(Store::open(root.path()));
        store.database_path().to_path_buf()
    };
    let connection = must(Connection::open(&database_path));
    must(connection.pragma_update(None, "foreign_keys", true));
    must(connection.execute(
        "INSERT INTO loom_agent_types(
             id, rev, digest, record_json, created_at_ms, updated_at_ms
         ) VALUES (?1, 1, ?2, '{}', 10, 10)",
        params!["researcher", "0123456789abcdef0123456789abcdef"],
    ));
    must(connection.execute(
        "INSERT INTO loom_cli_install_jobs(
             job_id, agent_type_id, agent_type_rev, agent_type_digest, state,
             total, completed, current_cli, error, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, 1, ?3, 'queued', 2, 0, NULL, NULL, 10, 10)",
        params![
            "install:researcher:1",
            "researcher",
            "0123456789abcdef0123456789abcdef"
        ],
    ));
    must(connection.execute(
        "INSERT INTO loom_cli_install_items(
             job_id, ordinal, cli_program, state, error, created_at_ms, updated_at_ms
         ) VALUES (?1, 0, 'rg', 'queued', NULL, 10, 10)",
        ["install:researcher:1"],
    ));
    assert!(
        connection
            .execute(
                "INSERT INTO loom_cli_install_items(
                     job_id, ordinal, cli_program, state, error, created_at_ms, updated_at_ms
                 ) VALUES (?1, 32, 'jq', 'unknown', NULL, 10, 10)",
                ["install:researcher:1"],
            )
            .is_err(),
        "migration must reject out-of-bounds ordinals and unknown states"
    );
    drop(connection);

    let reopened = must(Store::open(root.path()));
    assert_eq!(must(reopened.schema_version()), 26);
    let connection = must(Connection::open(reopened.database_path()));
    let retained: (String, u32, u32) = must(connection.query_row(
        "SELECT state, completed, total FROM loom_cli_install_jobs WHERE job_id = ?1",
        ["install:researcher:1"],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ));
    assert_eq!(retained, ("queued".into(), 0, 2));
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

/// MUTATION CHECK: drop the byte cut-off (or the at-least-one-envelope
/// progress guarantee) from `Store::read_page`. Expected failure: the
/// budgeted page returns all five envelopes, or the one-byte budget returns
/// an empty page and a byte-paged reader could stall.
/// Verified by revert on 2026-07-27.
#[test]
fn read_page_ends_early_on_byte_budget_and_always_makes_progress() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("read-page-budget");
    let mut batch = (1..=5)
        .map(|index| {
            envelope(
                &session,
                &format!("page-{index}"),
                json!({"type": "user_message", "text": format!("payload {index}")}),
            )
        })
        .collect::<Vec<_>>();
    must(store.append(&mut batch));

    // The replay page uses the same true-weight units as live catch-up.
    let row_weights = batch.iter().map(envelope_weight_bytes).collect::<Vec<_>>();
    let two_rows = row_weights[0] + row_weights[1];
    let first_page = must(store.read_page(&session, 0, 10, two_rows));
    assert_eq!(
        first_page
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        [1, 2],
        "the page ends before the third row would exceed the budget"
    );

    let resumed = must(store.read_page(&session, 2, 10, usize::MAX));
    assert_eq!(
        resumed
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        [3, 4, 5],
        "the next page resumes from the caller's last-received sequence"
    );

    let oversized = must(store.read_page(&session, 0, 10, 1));
    assert_eq!(
        oversized
            .iter()
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>(),
        [1],
        "a single envelope larger than the budget is still returned"
    );
}

fn test_payload_kind(envelope: &RawEnvelope) -> &str {
    let kind = envelope
        .payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind == "item"
        && envelope
            .payload
            .get("item")
            .and_then(|item| item.get("item"))
            .and_then(Value::as_str)
            == Some("tool_call")
    {
        "item_tool_call"
    } else {
        kind
    }
}

/// MUTATION CHECK: remove any reducer kind, omit the SQL predicate, or stop
/// paging across an over-selected legacy row. Expected failure: at least one
/// filtered reduction differs from the historical full-scan reduction, or an
/// irrelevant current row escapes the indexed reader.
#[test]
fn reducer_payload_filters_match_full_scan_for_every_declared_kind_set() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("reducer-payload-filter-equivalence");
    let payloads = [
        json!({"type": "unrelated_before"}),
        json!({"type": "run_state", "state": "queued"}),
        json!({"type": "user_message", "text": "hello", "mode": "start"}),
        json!({"type": "queue_changed", "revision": 4, "change": {"change": "unknown"}}),
        json!({"type": "run_retried", "failed_run_id": "failed", "prompt_run_id": "prompt", "user_seq": 3}),
        json!({"type": "run_failed", "message": "failed"}),
        json!({"type": "usage"}),
        json!({"type": "agent_spawned"}),
        json!({"type": "session_forked"}),
        json!({"type": "node_committed", "node": {"node": "node"}}),
        json!({"type": "effect", "phase": "dispatched", "effect": "effect"}),
        json!({"type": "menu_opened", "id": "menu"}),
        json!({"type": "menu_answered", "menu": "menu"}),
        json!({"type": "menu_closed", "menu": "menu"}),
        json!({"type": "task_completed"}),
        json!({"type": "item", "item": {"item": "command_execution"}}),
        json!({"type": "item", "item": {"item": "tool_call"}}),
        json!({"type": "tool_result", "call_id": "call"}),
        json!({"type": "process_signal_recorded", "effect_id": "effect"}),
        json!({"type": "unrelated_legacy"}),
        json!({"type": "unrelated_after"}),
    ];
    let mut batch = payloads
        .iter()
        .enumerate()
        .map(|(index, _)| {
            envelope(
                &session,
                &format!("reducer-filter-{index}"),
                json!({"type": "filter_fixture"}),
            )
        })
        .collect::<Vec<_>>();
    must(store.append(&mut batch));
    let connection = must(Connection::open(store.database_path()));
    for (envelope, payload) in batch.iter_mut().zip(payloads) {
        envelope.payload = payload;
        let kind = test_payload_kind(envelope).to_owned();
        let encoded = must(rmp_serde::to_vec_named(envelope));
        must(connection.execute(
            "UPDATE events
             SET envelope_json = ?2, payload_kind = ?3
             WHERE event_id = ?1",
            params![envelope.event_id.as_str(), encoded, kind],
        ));
    }
    let legacy_kinds = [
        "usage",
        "run_state",
        "queue_changed",
        "run_retried",
        "node_committed",
        "menu_closed",
        "tool_result",
        "unrelated_legacy",
    ];
    let legacy_event_ids = batch
        .iter()
        .filter(|envelope| legacy_kinds.contains(&test_payload_kind(envelope)))
        .map(|envelope| envelope.event_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(legacy_event_ids.len(), legacy_kinds.len());
    for event_id in &legacy_event_ids {
        must(connection.execute(
            "UPDATE events SET payload_kind = NULL WHERE event_id = ?1",
            [event_id.as_str()],
        ));
    }
    drop(connection);

    let full_scan = must(store.read(&session, 0, usize::MAX));
    let reducer_kind_sets: &[(&str, &[&str])] = &[
        (
            "usage",
            &["usage", "agent_spawned", "run_failed", "session_forked"],
        ),
        ("run-state", &["run_state"]),
        ("queue", &["user_message", "queue_changed"]),
        ("run-prompt-source", &["user_message", "run_retried"]),
        (
            "failed-turn",
            &["user_message", "run_failed", "run_retried"],
        ),
        ("tree-head", &["node_committed"]),
        (
            "startup-recovery",
            &["effect", "menu_opened", "menu_answered", "menu_closed"],
        ),
        (
            "recovery-evidence",
            &[
                "effect",
                "task_completed",
                "item",
                "item_tool_call",
                "tool_result",
                "process_signal_recorded",
            ],
        ),
    ];
    for (name, payload_kinds) in reducer_kind_sets {
        let mut cursor = 0;
        let mut filtered_scan = Vec::new();
        loop {
            let page =
                must(store.read_reducer_page(&session, cursor, 2, usize::MAX, payload_kinds));
            if page.is_empty() {
                break;
            }
            assert!(page.iter().all(|envelope| {
                legacy_event_ids.contains(&envelope.event_id)
                    || payload_kinds.contains(&test_payload_kind(envelope))
            }));
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            filtered_scan.extend(page);
        }
        let filtered_output = filtered_scan
            .into_iter()
            .filter(|envelope| payload_kinds.contains(&test_payload_kind(envelope)))
            .collect::<Vec<_>>();
        let full_output = full_scan
            .iter()
            .filter(|envelope| payload_kinds.contains(&test_payload_kind(envelope)))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(filtered_output, full_output, "{name} reducer output");
    }
}

/// MUTATION CHECK: return the indexed decode error directly instead of
/// falling back through the historical reader. Expected failure: the first
/// page reports corruption rather than returning the preceding full-scan row.
#[test]
fn reducer_page_decode_failure_falls_back_to_full_scan() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("reducer-corrupt-fallback");
    let mut batch = vec![
        envelope(
            &session,
            "reducer-corrupt-prefix",
            json!({"type": "irrelevant"}),
        ),
        envelope(
            &session,
            "reducer-corrupt-target",
            json!({"type": "run_state", "state": "queued"}),
        ),
    ];
    must(store.append(&mut batch));
    let connection = must(Connection::open(store.database_path()));
    must(connection.execute(
        "UPDATE events SET envelope_json = X'C1' WHERE event_id = ?1",
        [batch[1].event_id.as_str()],
    ));
    drop(connection);

    let fallback = must(store.read_reducer_page(&session, 0, 1, usize::MAX, &["run_state"]));
    assert_eq!(fallback, vec![batch[0].clone()]);
    let filtered_error =
        must_err(store.read_reducer_page(&session, fallback[0].seq, 1, usize::MAX, &["run_state"]));
    let full_scan_error = must_err(store.read(&session, fallback[0].seq, 1));
    assert_eq!(filtered_error.code, ErrorCode::StoreCorrupt);
    assert_eq!(filtered_error, full_scan_error);
}

/// MUTATION CHECK: derive the checkpoint boundary from the last selected row
/// or fetch the irrelevant tail envelope to advance it. The filtered page
/// must expose the committed head as metadata while decoding only run-state.
#[test]
fn reducer_page_observes_an_irrelevant_suffix_without_materializing_it() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("reducer-observed-head");
    let mut batch = vec![
        envelope(
            &session,
            "reducer-observed-state",
            json!({"type": "run_state", "state": "queued"}),
        ),
        envelope(
            &session,
            "reducer-observed-irrelevant",
            json!({"type": "irrelevant"}),
        ),
    ];
    must(store.append(&mut batch));

    let first =
        must(store.read_reducer_page_with_boundary(&session, 0, 16, usize::MAX, &["run_state"]));
    assert_eq!(first.envelopes, vec![batch[0].clone()]);
    assert_eq!(
        first.observed_head,
        Some((batch[1].seq, batch[1].event_id.clone()))
    );

    let suffix = must(store.read_reducer_page_with_boundary(
        &session,
        batch[0].seq,
        16,
        usize::MAX,
        &["run_state"],
    ));
    assert!(suffix.envelopes.is_empty());
    assert_eq!(
        suffix.observed_head,
        Some((batch[1].seq, batch[1].event_id.clone()))
    );
}

/// v0.0.936 attention state: `seen_at_ms` is monotone under ANY candidate —
/// the SQL CASE keeps the greater durable value, so a wall-clock regression
/// can never resurrect an unseen dot — and a replayed command id returns the
/// exact original receipt without moving the timestamp.
///
/// MUTATION CHECK (executed): replace the CASE with a plain overwrite
/// (`SET seen_at_ms = ?2`) and the future-poked value regresses — the
/// monotone assertion fails.
#[test]
fn session_seen_is_monotone_and_replays_the_original_receipt() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-seen-law");
    let mut batch = [envelope(
        &session,
        "seen-law-seed",
        json!({"type": "user_message", "text": "seed"}),
    )];
    must(store.append(&mut batch));

    let command = haider_store::SessionSeenCommand {
        command_id: "seen-cmd-1".into(),
        request_digest: "seen-digest-1".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        event_id: EventId::new("seen-evt-1"),
        device_id: DeviceId::new("device-test"),
    };
    let first = match must(store.mark_session_seen(&command)) {
        haider_store::SessionSeenOutcome::Committed { seen, .. } => seen,
        other => panic!("expected committed, got {other:?}"),
    };
    assert!(first.seen_at_ms > 0);

    let replay = match must(store.mark_session_seen(&command)) {
        haider_store::SessionSeenOutcome::IdempotentReplay { seen } => seen,
        other => panic!("expected idempotent replay, got {other:?}"),
    };
    assert_eq!(
        replay, first,
        "replay must return the exact original receipt"
    );

    // Poke the durable value into the future; a later mark must PRESERVE the
    // greater timestamp (the SQL CASE is the monotonicity proof).
    let future = first.seen_at_ms + 3_600_000;
    let connection = must(Connection::open(store.database_path()));
    must(connection.execute(
        "UPDATE sessions SET seen_at_ms = ?2 WHERE id = ?1",
        params![session.as_str(), must(i64::try_from(future))],
    ));
    let second_command = haider_store::SessionSeenCommand {
        command_id: "seen-cmd-2".into(),
        request_digest: "seen-digest-2".into(),
        event_id: EventId::new("seen-evt-2"),
        ..command.clone()
    };
    let second = match must(store.mark_session_seen(&second_command)) {
        haider_store::SessionSeenOutcome::Committed { seen, .. } => seen,
        other => panic!("expected committed, got {other:?}"),
    };
    assert_eq!(
        second.seen_at_ms, future,
        "monotone: the greater durable value is preserved over a smaller now"
    );
}
