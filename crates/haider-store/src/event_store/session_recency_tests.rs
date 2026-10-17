#![allow(clippy::expect_used)]

use super::{SessionRecencyRow, Store};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn page_with_work_budget(store: &Store, session_count: usize) -> Vec<SessionRecencyRow> {
    // 256 instructions per roster row allows the indexed head probe and
    // top-100 ordering. Scanning every session's whole journal exceeds this
    // by orders of magnitude. No wall-clock or CPU-speed assumption is used.
    let budget = session_count * 256;
    let steps = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&steps);
    store
        .connection()
        .expect("store connection")
        .progress_handler(
            1_000,
            Some(move || counted.fetch_add(1_000, Ordering::Relaxed) + 1_000 > budget),
        )
        .expect("install SQLite work counter");
    let page = store.session_recency_page(None, 100);
    store
        .connection()
        .expect("store connection")
        .progress_handler(0, None::<fn() -> bool>)
        .expect("remove SQLite work counter");
    let observed = steps.load(Ordering::Relaxed);
    assert!(observed > 0, "work counter observes the actual query");
    assert!(
        observed <= budget,
        "indexed recency query exceeded bounded SQLite work: {observed} > {budget}"
    );
    eprintln!("recency query: sessions={session_count}, counted_steps={observed}, budget={budget}");
    page.expect("indexed recency query completes within its work budget")
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

/// Pin indexed head lookup work independently of scheduler delay.
/// The progress handler counts the actual production query, not a copied SQL plan.
#[test]
fn recency_page_of_5000_sessions_has_bounded_indexed_work() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    let mut connection = rusqlite::Connection::open(store.database_path()).expect("open sqlite");
    seed_recency_sessions(&mut connection, 0..400);
    seed_recency_events(&mut connection, 0..400, 10);
    let page_400 = page_with_work_budget(&store, 400);
    assert_eq!(page_400.len(), 100);

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

    drop(connection);
    let page = page_with_work_budget(&store, 5_000);

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
}
