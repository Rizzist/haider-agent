#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::SessionPermissionOverridesV1;
use haider_protocol::state::SessionState;
use haider_store::{
    EventStore, SessionCreateCommand, SessionCreateOutcome, SessionSelectModelCommand,
    SessionSelectModelOutcome, Store,
};
use serde_json::json;
use std::sync::{Arc, Barrier};

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
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new(event_id),
        device_id: DeviceId::new("daemon-test"),
    }
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

#[test]
fn resolved_account_route_advances_only_the_served_sessions_epoch() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    let served = SessionId::new("served");
    let parked = SessionId::new("parked");
    let device = DeviceId::new("daemon-test");
    store
        .create_session(&command("create-served", "served", "created-served"))
        .expect("served");
    store
        .create_session(&command("create-parked", "parked", "created-parked"))
        .expect("parked");
    assert_eq!(
        store
            .commit_resolved_route(&served, "fake", "fake-v1", 0, Some("account-a"), &device)
            .expect("first route"),
        0
    );
    assert_eq!(
        store
            .commit_resolved_route(&served, "fake", "fake-v1", 0, Some("account-a"), &device)
            .expect("same route"),
        0
    );
    let changed = store
        .commit_resolved_route(&served, "fake", "fake-v1", 0, Some("account-b"), &device)
        .expect("switched route");
    assert!(changed > 0);
    let route_fact = store
        .read(&served, 0, 16)
        .expect("route journal")
        .into_iter()
        .find_map(|event| {
            haider_protocol::session::ModelSelected::from_payload_value(&event.payload)
        })
        .expect("route fact");
    assert!(
        route_fact.route_only,
        "same-pair route facts are not model notes"
    );
    assert_eq!(
        store
            .commit_resolved_route(
                &served,
                "fake",
                "fake-v1",
                changed,
                Some("account-b"),
                &device
            )
            .expect("same switched route"),
        changed
    );
    assert_eq!(
        store
            .commit_resolved_route(&served, "fake", "fake-v1", 0, Some("account-a"), &device)
            .expect("stale request"),
        0
    );
    let served_metadata = store
        .session_metadata(&served)
        .expect("served metadata")
        .expect("served row");
    assert_eq!(served_metadata.selection_epoch, Some(changed));
    assert_eq!(
        served_metadata.resolved_route_alias.as_deref(),
        Some("account-b")
    );
    assert_eq!(
        served_metadata.account_alias, None,
        "route tracking must not pin rotation"
    );
    let selection = SessionSelectModelCommand {
        command_id: "select-after-route".into(),
        request_digest: "select-after-route-digest".into(),
        request_json: "{}".into(),
        session_id: served.clone(),
        worker_generation: store.worker_generation(),
        provider: "fake".into(),
        model: "fake-v2".into(),
        expected_pair: None,
        automatic_run: None,
        account_alias: None,
        output_budget: None,
        event_id: EventId::new("selected-after-route"),
        device_id: DeviceId::new("daemon-test"),
    };
    let SessionSelectModelOutcome::Committed { selected, envelope } = store
        .select_session_model(&selection)
        .expect("model after route")
    else {
        panic!("selection was not committed")
    };
    assert!(selected.selected_seq > changed);
    assert!(
        !haider_protocol::session::ModelSelected::from_payload_value(&envelope.payload)
            .expect("explicit model fact")
            .route_only
    );
    let parked_metadata = store
        .session_metadata(&parked)
        .expect("parked metadata")
        .expect("parked row");
    assert_eq!(parked_metadata.selection_epoch, Some(0));
    assert_eq!(parked_metadata.resolved_route_alias, None);

    let keyless = SessionId::new("keyless");
    store
        .create_session(&command("create-keyless", "keyless", "created-keyless"))
        .expect("keyless");
    assert_eq!(
        store
            .commit_resolved_route(&keyless, "fake", "fake-v1", 0, None, &device)
            .expect("observe keyless route"),
        0
    );
    assert!(
        store
            .session_metadata(&keyless)
            .expect("keyless metadata")
            .expect("keyless row")
            .resolved_route_seen
    );
    let keyed = store
        .commit_resolved_route(&keyless, "fake", "fake-v1", 0, Some("account-a"), &device)
        .expect("keyed route after keyless");
    assert!(keyed > 0);
    let keyless_again = store
        .commit_resolved_route(&keyless, "fake", "fake-v1", keyed, None, &device)
        .expect("keyless route after keyed");
    assert!(keyless_again > keyed);
}

#[test]
fn legacy_session_mints_an_epoch_on_first_metadata_load() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("open");
    let session = SessionId::new("legacy-epoch");
    store
        .create_session(&command("create-legacy", "legacy-epoch", "created-legacy"))
        .expect("create");
    let connection = rusqlite::Connection::open(store.database_path()).expect("sqlite");
    connection.execute(
        "UPDATE sessions SET meta_json = json_remove(meta_json, '$.selection_epoch') WHERE id = ?1",
        [session.as_str()],
    ).expect("simulate pre-epoch metadata");
    let metadata = store
        .session_metadata(&session)
        .expect("load")
        .expect("metadata");
    assert_eq!(metadata.selection_epoch, Some(1));
    assert_eq!(
        store
            .session_metadata(&session)
            .expect("reload")
            .expect("metadata")
            .selection_epoch,
        Some(1)
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
