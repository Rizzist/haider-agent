#![allow(clippy::expect_used)]

use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_store::{EventStore, ReducerPageCursor, SessionProjectionCheckpoint, Store};
use rusqlite::Connection;
use serde_json::json;

fn envelope(session_id: &SessionId, event_id: &str, text: &str) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("authority-trigger-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: json!({"type": "user_message", "text": text}).into(),
    }
}

fn mutation_generation(connection: &Connection, session_id: &SessionId) -> i64 {
    connection
        .query_row(
            "SELECT journal_mutation_generation FROM sessions WHERE id = ?1",
            [session_id.as_str()],
            |row| row.get(0),
        )
        .expect("read journal mutation generation")
}

fn authority_trigger_count(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'trigger'
                AND name IN ('events_authority_updated', 'events_authority_deleted')",
            [],
            |row| row.get(0),
        )
        .expect("count authority triggers")
}

#[test]
fn payload_kind_membership_change_advances_reducer_authority() {
    let root = tempfile::tempdir().expect("profile root");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("payload-kind-authority");
    let mut events = vec![
        envelope(&session_id, "payload-kind-1", "one"),
        envelope(&session_id, "payload-kind-2", "two"),
    ];
    store.append(&mut events).expect("append fixture");

    let before = store
        .read_reducer_page_with_boundary(
            &session_id,
            ReducerPageCursor::after(0),
            usize::MAX,
            usize::MAX,
            &["user_message"],
        )
        .expect("read initial reducer page");
    assert_eq!(before.envelopes.len(), 2);

    let connection = Connection::open(store.database_path()).expect("open raw store");
    connection
        .execute(
            "UPDATE events SET payload_kind = 'excluded_fixture'
              WHERE session_id = ?1 AND seq = 1",
            [session_id.as_str()],
        )
        .expect("change projection membership");
    drop(connection);

    let after = store
        .read_reducer_page_with_boundary(
            &session_id,
            ReducerPageCursor::after(0),
            usize::MAX,
            usize::MAX,
            &["user_message"],
        )
        .expect("read changed reducer page");
    assert_eq!(after.envelopes.len(), 1);
    assert_ne!(
        after.observed_mutation_generation, before.observed_mutation_generation,
        "a projection-visible payload_kind change must advance authority"
    );
}

#[test]
fn open_repairs_rebuild_lost_triggers_and_invalidates_projection_authority_once() {
    let root = tempfile::tempdir().expect("profile root");
    let session_id = SessionId::new("trigger-rebuild-authority");
    let mut events = vec![
        envelope(&session_id, "trigger-rebuild-1", "one"),
        envelope(&session_id, "trigger-rebuild-2", "two"),
    ];
    let database_path = {
        let store = Store::open(root.path()).expect("open store");
        store.append(&mut events).expect("append fixture");
        store
            .put_session_projection_checkpoint(&SessionProjectionCheckpoint {
                session_id: session_id.clone(),
                projection: "prompt_history".into(),
                timeline_key: "trigger-rebuild".into(),
                through_seq: events[1].seq,
                boundary_event_id: events[1].event_id.clone(),
                payload: b"untrusted-prefix".to_vec(),
            })
            .expect("persist projection checkpoint");
        store.database_path().to_path_buf()
    };

    let connection = Connection::open(&database_path).expect("open rebuild connection");
    connection
        .pragma_update(None, "foreign_keys", false)
        .expect("disable foreign keys for SQLite table rebuild");
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE events_rebuilt (
                 session_id      TEXT NOT NULL,
                 seq             INTEGER NOT NULL CHECK (seq > 0),
                 envelope_json   TEXT NOT NULL,
                 event_id        TEXT NOT NULL UNIQUE,
                 committed_at_ms INTEGER NOT NULL,
                 payload_kind    TEXT,
                 PRIMARY KEY (session_id, seq),
                 FOREIGN KEY (session_id) REFERENCES sessions(id)
             );
             INSERT INTO events_rebuilt
             SELECT session_id, seq, envelope_json, event_id, committed_at_ms, payload_kind
               FROM events;
             DROP TABLE events;
             ALTER TABLE events_rebuilt RENAME TO events;
             CREATE INDEX events_payload_kind_session_seq
                 ON events(payload_kind, session_id, seq);
             COMMIT;",
        )
        .expect("rebuild events without recreating triggers");
    assert_eq!(authority_trigger_count(&connection), 0);
    assert_eq!(mutation_generation(&connection, &session_id), 0);
    drop(connection);

    let repaired = Store::open(root.path()).expect("reopen rebuilt store");
    let repaired_connection =
        Connection::open(repaired.database_path()).expect("inspect repaired store");
    assert_eq!(authority_trigger_count(&repaired_connection), 2);
    assert_eq!(
        mutation_generation(&repaired_connection, &session_id),
        1,
        "an unguarded interval advances authority exactly once"
    );
    assert_eq!(
        repaired
            .session_projection_checkpoint(&session_id, "prompt_history", "trigger-rebuild",)
            .expect("load invalidated checkpoint"),
        None,
        "projection checkpoints from the unguarded interval cannot be trusted"
    );
    let rebuilt_page = repaired
        .read_reducer_page_with_boundary(
            &session_id,
            ReducerPageCursor::after(0),
            usize::MAX,
            usize::MAX,
            &["user_message"],
        )
        .expect("rebuild reducer projection");
    assert_eq!(rebuilt_page.envelopes.len(), 2);
    assert_eq!(rebuilt_page.observed_mutation_generation, Some(1));
    repaired
        .put_session_projection_checkpoint(&SessionProjectionCheckpoint {
            session_id: session_id.clone(),
            projection: "prompt_history".into(),
            timeline_key: "trigger-rebuild".into(),
            through_seq: events[1].seq,
            boundary_event_id: events[1].event_id.clone(),
            payload: b"rebuilt-prefix".to_vec(),
        })
        .expect("persist rebuilt checkpoint");
    drop(repaired_connection);
    drop(repaired);

    let normal_reopen = Store::open(root.path()).expect("normal reopen");
    let normal_connection =
        Connection::open(normal_reopen.database_path()).expect("inspect normal reopen");
    assert_eq!(mutation_generation(&normal_connection, &session_id), 1);
    assert!(
        normal_reopen
            .session_projection_checkpoint(&session_id, "prompt_history", "trigger-rebuild",)
            .expect("load rebuilt checkpoint")
            .is_some(),
        "the healthy fast path must not invalidate trusted projections"
    );
}
