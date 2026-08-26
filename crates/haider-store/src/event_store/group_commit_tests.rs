use super::*;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets};
use rusqlite::hooks::{Action, AuthAction, AuthContext, Authorization};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const CRASH_CHILD_PROFILE_ENV: &str = "HAIDER_GROUP_COMMIT_CRASH_CHILD_PROFILE";
const CRASH_AFTER_SECOND_INSERT: i32 = 86;

fn envelope(session: &SessionId, event_id: impl Into<String>) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: u64::MAX,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("group-commit-test-device"),
        authority_epoch: 1,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: u64::MAX,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::json!({"type": "group_commit_test"}),
    }
}

fn batch(session: &SessionId, event_id: impl Into<String>) -> JournalAppendBatch {
    JournalAppendBatch {
        envelopes: vec![envelope(session, event_id)],
        validate_worker_transitions: false,
    }
}

fn install_commit_counter(store: &Store) -> Arc<AtomicUsize> {
    let commits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&commits);
    store
        .connection()
        .expect("journal connection")
        .commit_hook(Some(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .expect("install commit counter");
    commits
}

fn event_count(path: &Path) -> i64 {
    Connection::open(path)
        .expect("open observer")
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("count events")
}

/// MUTATION CHECK: send a request completion before `transaction.commit()`,
/// or stamp the caller-owned envelopes before that boundary. Expected runtime
/// failure: completion becomes observable while SQLite's commit hook is still
/// paused, or uncommitted events leak through a second WAL reader.
#[test]
fn append_group_never_acknowledges_before_the_shared_commit() {
    let root = tempfile::tempdir().expect("profile");
    let store = Arc::new(Store::open(root.path()).expect("store"));
    let database_path = store.database_path().to_path_buf();
    let session_a = SessionId::new("group-commit-ack-a");
    let session_b = SessionId::new("group-commit-ack-b");
    let (at_commit_tx, at_commit_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);

    store
        .connection()
        .expect("journal connection")
        .commit_hook(Some(move || {
            if at_commit_tx.send(()).is_err() {
                return true;
            }
            release_rx.recv().is_err()
        }))
        .expect("install commit barrier");

    let writer = Arc::clone(&store);
    let (completed_tx, completed_rx) = mpsc::sync_channel(0);
    let handle = std::thread::spawn(move || {
        let mut batches = vec![batch(&session_a, "ack-a"), batch(&session_b, "ack-b")];
        let result = writer.append_group(&mut batches);
        let _ = completed_tx.send((result, batches));
    });

    at_commit_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer reaches shared commit");
    assert!(
        completed_rx.try_recv().is_err(),
        "no request may complete before the shared commit returns"
    );
    assert_eq!(
        event_count(&database_path),
        0,
        "a separate WAL reader must not observe uncommitted group rows"
    );

    release_tx.send(()).expect("release commit");
    let (result, batches) = completed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("writer completes after commit");
    let outcomes = result.expect("group commits");
    assert!(outcomes.iter().all(Result::is_ok));
    assert_eq!(batches[0].envelopes[0].seq, 1);
    assert_eq!(batches[1].envelopes[0].seq, 1);
    assert_eq!(event_count(&database_path), 2);
    handle.join().expect("writer joins");
}

/// MUTATION CHECK: commit a savepoint or individual request as durable truth
/// before the outer group commit. Expected runtime failure: rejecting the
/// outer commit leaves any event behind, or mutates any caller-owned envelope.
#[test]
fn rejected_outer_commit_rolls_back_the_whole_group_atomically() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let reject_once = Arc::new(AtomicBool::new(true));
    let reject = Arc::clone(&reject_once);
    store
        .connection()
        .expect("journal connection")
        .commit_hook(Some(move || reject.swap(false, Ordering::SeqCst)))
        .expect("install rejecting commit hook");

    let session_a = SessionId::new("group-commit-rollback-a");
    let session_b = SessionId::new("group-commit-rollback-b");
    let mut batches = vec![
        batch(&session_a, "rollback-a-1"),
        batch(&session_b, "rollback-b-1"),
        batch(&session_a, "rollback-a-2"),
    ];
    let unstamped = batches
        .iter()
        .map(|batch| batch.envelopes.clone())
        .collect::<Vec<_>>();

    let error = store
        .append_group(&mut batches)
        .expect_err("outer commit is rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(event_count(store.database_path()), 0);
    assert_eq!(
        batches
            .iter()
            .map(|batch| batch.envelopes.clone())
            .collect::<Vec<_>>(),
        unstamped,
        "failed commit must not expose speculative stamps"
    );

    let outcomes = store
        .append_group(&mut batches)
        .expect("the intact group retries");
    assert!(outcomes.iter().all(Result::is_ok));
    assert_eq!(event_count(store.database_path()), 3);
}

/// MUTATION CHECK: commit released savepoints as independent durable writes.
/// Expected runtime failure: after the child process exits immediately after
/// SQLite accepts the second event insert, reopening the WAL exposes a partial
/// group instead of rolling the whole uncommitted transaction back.
#[test]
fn crash_mid_group_loses_the_whole_group_atomically() {
    if let Some(profile) = std::env::var_os(CRASH_CHILD_PROFILE_ENV) {
        let store = Store::open(PathBuf::from(profile)).expect("crash-child store");
        let inserted_events = Arc::new(AtomicUsize::new(0));
        let observed_inserts = Arc::clone(&inserted_events);
        store
            .connection()
            .expect("crash-child journal connection")
            .update_hook(Some(
                move |_action: Action, _database: &str, table: &str, _row_id: i64| {
                    if table == "events" && observed_inserts.fetch_add(1, Ordering::SeqCst) + 1 == 2
                    {
                        std::process::exit(CRASH_AFTER_SECOND_INSERT);
                    }
                },
            ))
            .expect("install crash hook");

        let session = SessionId::new("group-commit-crashed");
        let mut batches = (0..4)
            .map(|index| batch(&session, format!("crashed-{index}")))
            .collect::<Vec<_>>();
        let _ = store.append_group(&mut batches);
        std::process::exit(CRASH_AFTER_SECOND_INSERT + 1);
    }

    let root = tempfile::tempdir().expect("profile");
    let status = std::process::Command::new(std::env::current_exe().expect("current test binary"))
        .arg("crash_mid_group_loses_the_whole_group_atomically")
        .arg("--test-threads=1")
        .env(CRASH_CHILD_PROFILE_ENV, root.path())
        .status()
        .expect("run crash child");
    assert_eq!(
        status.code(),
        Some(CRASH_AFTER_SECOND_INSERT),
        "the child must exit from the hook after SQLite accepts two inserts"
    );

    let session = SessionId::new("group-commit-crashed");
    let store = Store::open(root.path()).expect("reopen crashed store");
    assert_eq!(event_count(store.database_path()), 0);
    assert_eq!(store.latest_seq(&session).expect("recovered head"), 0);
}

/// MUTATION CHECK: sort the queued requests by session or return results in a
/// different order. Expected runtime failure: rowid order or per-session
/// sequences no longer match the interleaved arrival order.
#[test]
fn queued_concurrent_sessions_commit_in_arrival_order() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_a = SessionId::new("group-commit-order-a");
    let session_b = SessionId::new("group-commit-order-b");
    let mut batches = vec![
        batch(&session_a, "order-a-1"),
        batch(&session_b, "order-b-1"),
        batch(&session_a, "order-a-2"),
        batch(&session_b, "order-b-2"),
    ];

    let outcomes = store.append_group(&mut batches).expect("group commits");
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| {
                let range = outcome.as_ref().expect("request commits");
                (range.session_id.clone(), range.first_seq, range.last_seq)
            })
            .collect::<Vec<_>>(),
        [
            (session_a.clone(), 1, 1),
            (session_b.clone(), 1, 1),
            (session_a.clone(), 2, 2),
            (session_b.clone(), 2, 2),
        ]
    );
    let connection = Connection::open(store.database_path()).expect("observer");
    let event_ids = connection
        .prepare("SELECT event_id FROM events ORDER BY rowid")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("read commit order");
    assert_eq!(
        event_ids,
        ["order-a-1", "order-b-1", "order-a-2", "order-b-2"]
    );
}

/// MUTATION CHECK: remove the singleton fast path and send one request through
/// the multi-request savepoint loop. Expected runtime failure: SQLite observes
/// a SAVEPOINT instead of the direct one-transaction append shape.
#[test]
fn lone_append_group_commits_immediately_without_a_batching_wait() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let commits = install_commit_counter(&store);
    let savepoints = Arc::new(AtomicUsize::new(0));
    let observed_savepoints = Arc::clone(&savepoints);
    store
        .connection()
        .expect("journal connection")
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Savepoint { .. }) {
                observed_savepoints.fetch_add(1, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .expect("install savepoint observer");
    let session = SessionId::new("group-commit-single");
    let mut batches = vec![batch(&session, "single")];

    let outcomes = store.append_group(&mut batches).expect("single commits");
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(savepoints.load(Ordering::SeqCst), 0);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].as_ref().expect("request commits").first_seq, 1);
    assert_eq!(batches[0].envelopes[0].seq, 1);
    assert_eq!(event_count(store.database_path()), 1);
}

/// MUTATION CHECK: move the outer transaction into the request loop. Expected
/// runtime failure: a loaded group consumes one SQLite commit per request
/// instead of one commit for the entire already-available queue.
#[test]
fn loaded_group_uses_one_commit_instead_of_one_per_append() {
    const APPENDS: usize = 100;

    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let commits = install_commit_counter(&store);
    let session = SessionId::new("group-commit-loaded");
    let mut batches = (0..APPENDS)
        .map(|index| batch(&session, format!("loaded-{index}")))
        .collect::<Vec<_>>();

    let outcomes = store
        .append_group(&mut batches)
        .expect("loaded group commits");
    assert!(outcomes.iter().all(Result::is_ok));
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(event_count(store.database_path()), APPENDS as i64);
    assert_eq!(batches[0].envelopes[0].seq, 1);
    assert_eq!(batches[APPENDS - 1].envelopes[0].seq, APPENDS as u64);
}

/// MUTATION CHECK: special-case singleton errors as outer transaction
/// failures. Expected runtime failure: one bad request changes result shape
/// depending only on whether another request happened to be queued.
#[test]
fn singleton_semantic_failure_keeps_the_per_request_result_shape() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session = SessionId::new("group-commit-single-error");
    let mut batches = vec![JournalAppendBatch {
        envelopes: Vec::new(),
        validate_worker_transitions: false,
    }];

    let outcomes = store
        .append_group(&mut batches)
        .expect("semantic error is request-local");
    assert_eq!(outcomes.len(), 1);
    let error = outcomes[0].as_ref().expect_err("empty request is rejected");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(store.latest_seq(&session).expect("empty head"), 0);
}
