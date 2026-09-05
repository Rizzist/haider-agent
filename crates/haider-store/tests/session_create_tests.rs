#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::SessionState;
use haider_store::{EventStore, SessionCreateCommand, SessionCreateOutcome, Store};
use serde_json::json;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn command(command_id: &str, session_id: &str, event_id: &str) -> SessionCreateCommand {
    SessionCreateCommand {
        command_id: command_id.into(),
        request_digest: "digest-a".into(),
        request_json:
            r#"{"cwd":"/tmp/work","max_tokens":4096,"model":"fake-v1","provider":"fake"}"#.into(),
        session_id: SessionId::new(session_id),
        cwd: "/tmp/work".into(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new(event_id),
        device_id: DeviceId::new("daemon-test"),
    }
}

fn seed_recency_sessions(connection: &mut rusqlite::Connection, range: std::ops::Range<u64>) {
    let transaction = connection.transaction().expect("begin seed transaction");
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO sessions(id, created_at_ms, meta_json, seen_at_ms) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .expect("prepare session seed");
        for index in range {
            let session_id = format!("session-{index:04}");
            let created_at_ms = 1_000_u64 + index;
            let seen_at_ms = (index % 10 == 0).then_some(10_000_u64 + index);
            let meta_json = if index % 2 == 0 {
                r#"{"title":"zzz"}"#
            } else {
                r#"{"title":"aaa"}"#
            };
            insert
                .execute(rusqlite::params![
                    session_id,
                    i64::try_from(created_at_ms).expect("created fits"),
                    meta_json,
                    seen_at_ms.map(|value| i64::try_from(value).expect("seen fits")),
                ])
                .expect("seed session");
        }
    }
    transaction.commit().expect("commit seed transaction");
}

fn seed_recency_events(
    connection: &mut rusqlite::Connection,
    range: std::ops::Range<u64>,
    events_per_session: u64,
) {
    let transaction = connection.transaction().expect("begin event seed");
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO events( \
                     session_id, seq, envelope_json, event_id, committed_at_ms \
                 ) VALUES (?1, ?2, '{}', ?3, ?4)",
            )
            .expect("prepare event seed");
        for index in range {
            for seq in 1..=events_per_session {
                insert
                    .execute(rusqlite::params![
                        format!("session-{index:04}"),
                        i64::try_from(seq).expect("seq fits"),
                        format!("event-{index:04}-{seq:02}"),
                        i64::try_from(5_000 + index + seq).expect("timestamp fits"),
                    ])
                    .expect("seed event");
            }
        }
    }
    transaction.commit().expect("commit event seed");
}

/// Cost pin for the P0 launcher path. The timed region is only the production
/// indexed recency query; fixture creation is deliberately outside it.
#[test]
fn recency_page_of_5000_sessions_is_indexed_and_under_500_ms() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    let mut connection = rusqlite::Connection::open(store.database_path()).expect("open sqlite");
    seed_recency_sessions(&mut connection, 0..400);
    seed_recency_events(&mut connection, 0..400, 10);
    let started_400 = Instant::now();
    let page_400 = store
        .session_recency_page(None, 100)
        .expect("400-session recency page");
    let elapsed_400 = started_400.elapsed();
    assert_eq!(page_400.len(), 100);
    assert!(
        elapsed_400 < Duration::from_millis(100),
        "400-session recency page exceeded 100 ms: {elapsed_400:?}"
    );

    seed_recency_sessions(&mut connection, 400..5_000);
    seed_recency_events(&mut connection, 400..5_000, 10);
    let transaction = connection.transaction().expect("begin head transaction");
    transaction
        .execute(
            "INSERT INTO events( \
                 session_id, seq, envelope_json, event_id, committed_at_ms \
             ) VALUES ('session-4998', 11, '{}', 'head-wins', 80000),
                    ('session-4999', 11, '{}', 'latest-head', 90000)",
            [],
        )
        .expect("seed latest head");
    transaction
        .execute(
            "UPDATE sessions SET created_at_ms = 60000 WHERE id = 'session-4996'",
            [],
        )
        .expect("seed created winner");
    transaction
        .execute(
            "UPDATE sessions SET seen_at_ms = 70000 WHERE id = 'session-4997'",
            [],
        )
        .expect("seed seen winner");
    transaction
        .execute(
            "UPDATE sessions SET seen_at_ms = 100000 \
             WHERE id IN ('session-0003', 'session-0004')",
            [],
        )
        .expect("seed equal-recency tie");
    transaction.commit().expect("commit seed transaction");

    let query_plan = {
        let mut statement = connection
            .prepare(
                "EXPLAIN QUERY PLAN \
                 WITH roster AS ( \
                     SELECT s.id, max( \
                         s.created_at_ms, coalesce(s.seen_at_ms, 0), \
                         coalesce(head.committed_at_ms, 0) \
                     ) AS last_activity_ms \
                     FROM sessions AS s \
                     LEFT JOIN events AS head ON head.rowid = ( \
                         SELECT rowid FROM events \
                         WHERE session_id = s.id ORDER BY seq DESC LIMIT 1 \
                     ) \
                 ) \
                 SELECT id, last_activity_ms FROM roster \
                 ORDER BY last_activity_ms DESC, id ASC LIMIT 100",
            )
            .expect("prepare query plan");
        statement
            .query_map([], |row| row.get::<_, String>(3))
            .expect("query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect query plan")
    };
    assert!(
        query_plan.iter().any(|detail| {
            detail.contains("SEARCH events USING COVERING INDEX sqlite_autoindex_events_")
                && detail.contains("session_id")
        }),
        "head lookup must use the (session_id, seq) primary-key index: {query_plan:?}"
    );
    drop(connection);

    let started = Instant::now();
    let page = store.session_recency_page(None, 100).expect("recency page");
    let elapsed = started.elapsed();

    assert_eq!(page.len(), 100);
    assert_eq!(page[0].key.session_id.as_str(), "session-0003");
    assert_eq!(page[0].key.last_activity_ms, 100_000);
    assert_eq!(page[1].key.session_id.as_str(), "session-0004");
    assert_eq!(page[1].key.last_activity_ms, 100_000);
    assert_eq!(page[2].key.session_id.as_str(), "session-4999");
    assert_eq!(page[2].key.last_activity_ms, 90_000);
    assert_eq!(page[3].key.session_id.as_str(), "session-4998");
    assert_eq!(page[3].key.last_activity_ms, 80_000);
    assert_eq!(page[4].key.session_id.as_str(), "session-4997");
    assert_eq!(page[4].key.last_activity_ms, 70_000);
    assert_eq!(page[5].key.session_id.as_str(), "session-4996");
    assert_eq!(page[5].key.last_activity_ms, 60_000);
    assert!(
        elapsed < Duration::from_millis(500),
        "5,000-session recency page exceeded 500 ms: {elapsed:?}"
    );
    eprintln!(
        "recency page: 400 sessions {elapsed_400:?} (budget 100ms); \
         5,000 sessions {elapsed:?} (budget 500ms)"
    );
}

/// MUTATION CHECK: remove the same-command committed-receipt lookup from
/// `create_session`. Expected failure: the reopen retry attempts a second
/// session insert instead of returning the original response coordinates.
#[test]
fn session_create_commits_metadata_created_and_receipt_atomically_and_replays_after_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = {
        let store = Store::open(root.path()).expect("open");
        let outcome = store
            .create_session(&command("create-1", "session-1", "created-1"))
            .expect("create");
        let SessionCreateOutcome::Committed { created, envelope } = outcome else {
            panic!("first create must commit");
        };
        assert_eq!(created.created_seq, 1);
        assert_eq!(created.metadata.cwd, "/tmp/work");
        assert_eq!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone().into())
                .expect("payload"),
            EventPayload::SessionState(SessionState::Created)
        );
        assert_eq!(
            store
                .session_metadata(&created.session_id)
                .expect("metadata"),
            Some(created.metadata.clone())
        );
        assert_eq!(
            store.read(&created.session_id, 0, 10).expect("events"),
            vec![*envelope]
        );
        created
    };

    let reopened = Store::open(root.path()).expect("reopen");
    let replay = reopened
        .create_session(&command("create-1", "ignored-session", "ignored-event"))
        .expect("idempotent replay");
    assert_eq!(
        replay,
        SessionCreateOutcome::IdempotentReplay {
            created: first.clone()
        }
    );
    assert_eq!(
        reopened.session_ids().expect("session ids"),
        [first.session_id]
    );
}

/// MUTATION CHECK: compare only `command_id` and omit the method/digest/body
/// checks. Expected failure: the changed semantic request below recovers the
/// old response instead of returning `InvalidArgument`.
#[test]
fn same_session_create_command_with_different_body_is_rejected() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    store
        .create_session(&command("create-1", "session-1", "created-1"))
        .expect("first create");
    let mut changed = command("create-1", "session-2", "created-2");
    changed.request_digest = "digest-b".into();
    changed.request_json =
        r#"{"cwd":"/tmp/other","max_tokens":4096,"model":"fake-v1","provider":"fake"}"#.into();
    changed.cwd = "/tmp/other".into();
    let error = store
        .create_session(&changed)
        .expect_err("different body must fail");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
    assert_eq!(
        store.session_ids().expect("session ids"),
        [SessionId::new("session-1")]
    );
}

/// MUTATION CHECK: omit the override from metadata persistence or from the
/// canonical create digest/body. Expected RUNTIME failure: reopen loses the
/// allow flags or a same-command request with different flags replays the old
/// receipt instead of being rejected.
#[test]
fn session_create_persists_permission_overrides_and_binds_them_to_the_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let expected = SessionPermissionOverridesV1 {
        read_only: false,
        allow_writes: true,
        allow_exec: false,
        allow_mobile: false,
        auto_allow: false,
    };
    {
        let store = Store::open(root.path()).expect("open");
        let mut create = command("create-overrides", "session-overrides", "created-overrides");
        create.permission_overrides = Some(expected);
        create.request_digest = "digest-with-overrides".into();
        create.request_json = r#"{"cwd":"/tmp/work","max_tokens":4096,"model":"fake-v1","permission_overrides":{"allow_exec":false,"allow_writes":true},"provider":"fake"}"#.into();
        let SessionCreateOutcome::Committed { created, .. } = store
            .create_session(&create)
            .expect("create with overrides")
        else {
            panic!("first create commits");
        };
        assert_eq!(created.metadata.permission_overrides, Some(expected));
    }

    let reopened = Store::open(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .session_metadata(&SessionId::new("session-overrides"))
            .expect("metadata")
            .and_then(|metadata| metadata.permission_overrides),
        Some(expected)
    );
    let different_flags = command("create-overrides", "ignored", "ignored-event");
    let error = reopened
        .create_session(&different_flags)
        .expect_err("same command with different override body must fail");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
}

/// MUTATION CHECK: perform receipt lookup outside the IMMEDIATE transaction.
/// Expected failure: both racing callers can commit distinct sessions for the
/// same durable command.
#[test]
fn concurrent_same_command_has_one_committed_session_and_one_idempotent_replay() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(root.path()).expect("open"));
    let barrier = Arc::new(Barrier::new(3));
    let handles = [
        command("create-race", "session-a", "created-a"),
        command("create-race", "session-b", "created-b"),
    ]
    .into_iter()
    .map(|command| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store.create_session(&command).expect("racing create")
        })
    })
    .collect::<Vec<_>>();
    barrier.wait();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("join"))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateOutcome::Committed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateOutcome::IdempotentReplay { .. }))
            .count(),
        1
    );
    assert_eq!(store.session_ids().expect("session ids").len(), 1);
}

#[test]
fn legacy_append_metadata_remains_none() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    let session_id = SessionId::new("legacy");
    let mut envelope = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("legacy-event"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("test"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: json!({"type": "legacy"}).into(),
    }];
    store.append(&mut envelope).expect("append");
    assert_eq!(store.session_metadata(&session_id).expect("metadata"), None);
}
