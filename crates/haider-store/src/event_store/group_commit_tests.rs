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

fn session_create_command(session_id: &SessionId, suffix: &str) -> SessionCreateCommand {
    SessionCreateCommand {
        command_id: format!("create-{suffix}"),
        request_digest: "create-digest".into(),
        request_json: r#"{"cwd":"/tmp","max_tokens":4096,"model":"fake-v1","provider":"fake"}"#
            .into(),
        session_id: session_id.clone(),
        cwd: "/tmp".into(),
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4_096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "group-commit-test-system".into(),
        event_id: EventId::new(format!("created-{suffix}")),
        device_id: DeviceId::new("group-commit-test-device"),
    }
}

fn create_typed_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&session_create_command(session_id, session_id.as_str()))
        .expect("create typed session");
}

fn turn_command(store: &Store, session_id: &SessionId, suffix: &str) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: format!("submit-{suffix}"),
        request_digest: "submit-digest".into(),
        request_json: format!(
            r#"{{"attachments":[],"mode":"queue","session_id":"{session_id}","text":"hello","worker_generation":{}}}"#,
            store.worker_generation()
        ),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("run-{suffix}")),
        agent_id: None,
        branch_id: None,
        text: "hello".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{suffix}")),
        user_event_id: EventId::new(format!("user-{suffix}")),
        active_event_id: EventId::new(format!("active-{suffix}")),
        device_id: DeviceId::new("group-commit-test-device"),
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
        .append_owned_group(&mut batches)
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

/// MUTATION CHECK: route either receipt kind or acknowledgements around the
/// group, or split a receipt from accepted envelopes. Expected runtime
/// failure: the commit count exceeds one, receipt/prefix truth disagrees, or
/// the handled outbox row remains pending.
#[test]
fn receipts_append_and_hook_ack_share_one_outer_commit() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let accepted_session = SessionId::new("group-commit-receipt");
    let appended_session = SessionId::new("group-commit-append");
    let acknowledged_session = SessionId::new("group-commit-ack");
    let created_session = SessionId::new("group-commit-created");
    create_typed_session(&store, &accepted_session);
    let mut acknowledged = [envelope(&acknowledged_session, "handled-before-group")];
    store
        .append(&mut acknowledged)
        .expect("seed hook outbox row");
    let commits = install_commit_counter(&store);

    let mut batches = vec![
        JournalCommitBatch::CreateSession {
            command: session_create_command(&created_session, "grouped"),
            interaction_mode: SessionInteractionModeV1::Interactive,
        },
        JournalCommitBatch::AcceptTurn {
            command: turn_command(&store, &accepted_session, "grouped"),
            peer_message: None,
            auto_title: None,
            validate_headless: false,
        },
        JournalCommitBatch::Append(batch(&appended_session, "grouped-append")),
        JournalCommitBatch::HookAcks(vec![(acknowledged_session.clone(), acknowledged[0].seq)]),
    ];
    let outcomes = store
        .commit_group(&mut batches)
        .expect("mixed group commits");

    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outcomes[0].as_ref().expect("creation commits"),
        JournalCommitOutcome::CreateSession(SessionCreateOutcome::Committed { .. })
    ));
    let accepted = match outcomes[1].as_ref().expect("acceptance commits") {
        JournalCommitOutcome::AcceptTurn(TurnAcceptOutcome::Committed {
            accepted,
            envelopes,
        }) => {
            assert_eq!(envelopes.len(), 4);
            accepted
        }
        _ => panic!("fresh acceptance returns its committed receipt and prefix"),
    };
    assert_eq!(accepted.accepted_seq, 3);
    assert!(matches!(
        outcomes[2].as_ref().expect("append commits"),
        JournalCommitOutcome::Append(_)
    ));
    assert!(matches!(
        outcomes[3].as_ref().expect("ack commits"),
        JournalCommitOutcome::HookAcks
    ));
    assert!(
        store
            .session_create_receipt(
                "create-grouped",
                "create-digest",
                &session_create_command(&created_session, "grouped").request_json,
            )
            .expect("create receipt lookup")
            .is_some(),
        "session-create receipt is durable with its Created event"
    );
    assert!(
        store
            .turn_accept_receipt(
                "submit-grouped",
                "submit-digest",
                &turn_command(&store, &accepted_session, "grouped").request_json
            )
            .expect("receipt lookup")
            .is_some(),
        "receipt is durable with the accepted prefix"
    );
    assert!(
        store
            .pending_hook_dispatches(16)
            .expect("pending hook work")
            .iter()
            .all(|envelope| envelope.event_id.as_str() != "handled-before-group"),
        "the handled row is removed by the shared commit"
    );
}

/// MUTATION CHECK: let one invalid receipt roll back the outer transaction,
/// or leak its pending receipt while preserving adjacent appends. Expected
/// runtime failure: valid neighbors disappear or an invalid receipt remains.
#[test]
fn invalid_receipt_is_savepoint_local_inside_a_mixed_group() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let receipt_session = SessionId::new("group-commit-invalid-receipt");
    let append_session = SessionId::new("group-commit-valid-neighbors");
    create_typed_session(&store, &receipt_session);
    let mut invalid = turn_command(&store, &receipt_session, "invalid");
    invalid.text.clear();
    let commits = install_commit_counter(&store);
    let mut batches = vec![
        JournalCommitBatch::Append(batch(&append_session, "neighbor-before")),
        JournalCommitBatch::AcceptTurn {
            command: invalid,
            peer_message: None,
            auto_title: None,
            validate_headless: false,
        },
        JournalCommitBatch::Append(batch(&append_session, "neighbor-after")),
    ];

    let outcomes = store
        .commit_group(&mut batches)
        .expect("outer group commits");
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(outcomes[0].is_ok());
    let Err(error) = &outcomes[1] else {
        panic!("invalid receipt is local");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(outcomes[2].is_ok());
    assert_eq!(store.latest_seq(&append_session).expect("neighbor head"), 2);
    assert!(
        store
            .turn_accept_receipt(
                "submit-invalid",
                "submit-digest",
                &turn_command(&store, &receipt_session, "invalid").request_json,
            )
            .expect("invalid receipt lookup")
            .is_none(),
        "savepoint rollback removes the invalid pending receipt"
    );
}
