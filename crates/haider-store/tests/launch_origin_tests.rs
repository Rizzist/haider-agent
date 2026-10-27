#![allow(clippy::expect_used)]
//! Launch-origin registration store laws
//! (`docs/design/dated-workspace-v1.md` §4: O2–O6).

use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::session::{
    LaunchOriginPathKindV1, LaunchOriginPathV1, SessionLaunchOriginSelected,
};
use haider_store::{
    EventStore, SessionCreateCommand, SessionLaunchOriginCommand, SessionLaunchOriginOutcome, Store,
};
use serde_json::json;
use std::fmt::Debug;
use std::path::Path;

fn must<T, E: Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("operation failed: {error:?}"),
    }
}

fn test_root() -> tempfile::TempDir {
    must(tempfile::tempdir())
}

fn home_path(display: &str) -> LaunchOriginPathV1 {
    LaunchOriginPathV1 {
        kind: LaunchOriginPathKindV1::HomeRelative,
        display: Some(display.to_string()),
    }
}

fn origin_command(
    store: &Store,
    command_id: &str,
    session: &SessionId,
    open_id: &str,
    expected_revision: u64,
    path: LaunchOriginPathV1,
) -> SessionLaunchOriginCommand {
    let request_json =
        format!("{{\"open_id\":{open_id:?},\"expected_revision\":{expected_revision}}}");
    SessionLaunchOriginCommand {
        command_id: command_id.into(),
        request_digest: format!("digest-{command_id}"),
        request_json,
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        open_id: open_id.into(),
        expected_revision,
        path,
        workspace_materialized: Some(false),
        event_id: EventId::new(format!("evt-{command_id}")),
        device_id: DeviceId::new("device-test"),
    }
}

fn seed_legacy_session(store: &Store, session: &SessionId) {
    let mut batch = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("origin-seed"),
        seq: 9_999,
        session_id: session.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("device-test"),
        authority_epoch: 1,
        worker_generation: 7,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Pruned,
        },
        payload: json!({"type": "user_message", "text": "seed"}).into(),
    }];
    must(store.append(&mut batch));
}

fn typed_session(store: &Store, session_id: &str, cwd: &Path) -> SessionId {
    let cwd = cwd.to_string_lossy().into_owned();
    let command = SessionCreateCommand {
        command_id: format!("create-{session_id}"),
        request_digest: "digest-create".into(),
        request_json: json!({
            "cwd": &cwd,
            "max_tokens": 4096,
            "model": "fake-v1",
            "provider": "fake"
        })
        .to_string(),
        session_id: SessionId::new(session_id),
        cwd,
        provider: "fake".into(),
        model: "fake-v1".into(),
        max_tokens: 4096,
        max_tokens_source: None,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new(format!("created-{session_id}")),
        device_id: DeviceId::new("daemon-test"),
    };
    must(store.create_session(&command));
    SessionId::new(session_id)
}

#[test]
fn copied_origin_projection_from_old_daemon_is_not_child_authority() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let parent = typed_session(&store, "old-origin-parent", root.path());
    let child = typed_session(&store, "old-origin-child", root.path());
    must(store.register_session_launch_origin(&origin_command(
        &store,
        "parent-origin",
        &parent,
        "parent-open",
        0,
        home_path("~/parent"),
    )));
    let parent_origin = must(store.latest_launch_origin(&parent)).expect("parent origin");
    let connection = must(rusqlite::Connection::open(store.database_path()));
    must(connection.execute(
        "UPDATE sessions SET meta_json = json_set(meta_json, '$.launch_origin', json(?1)) WHERE id = ?2",
        rusqlite::params![must(serde_json::to_string(&parent_origin)), child.as_str()],
    ));
    drop(connection);
    drop(store);
    let store = must(Store::open(root.path()));
    let before_metadata = must(store.session_metadata(&child)).expect("typed child");
    let before_origin = must(store.latest_launch_origin(&child));
    must(store.register_session_launch_origin(&origin_command(
        &store,
        "child-origin",
        &child,
        "child-open",
        0,
        home_path("~/child"),
    )));
    assert!(before_metadata.launch_origin.is_none());
    assert!(before_origin.is_none());
    let stale = origin_command(
        &store,
        "stale-child-origin",
        &child,
        "stale-open",
        0,
        home_path("~/stale"),
    );
    assert_eq!(
        store
            .register_session_launch_origin(&stale)
            .expect_err("real stale child revision rejected")
            .code,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        must(store.latest_launch_origin(&child))
            .expect("child origin")
            .subject_session_id,
        child.as_str()
    );
    assert_eq!(
        must(store.latest_launch_origin(&parent)),
        Some(parent_origin)
    );
}

fn derived_create_origin(mode: haider_protocol::session::SessionInteractionModeV1) {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let parent = typed_session(&store, "create-origin-parent", root.path());
    must(store.register_session_launch_origin(&origin_command(
        &store,
        "create-parent-origin",
        &parent,
        "parent-open",
        0,
        home_path("~/parent"),
    )));
    let parent_metadata = must(store.session_metadata(&parent)).expect("typed parent");
    let child = SessionId::new("create-origin-child");
    let command = SessionCreateCommand {
        command_id: "create-derived-child".into(),
        request_digest: "create-derived-digest".into(),
        request_json: "{}".into(),
        session_id: child.clone(),
        cwd: parent_metadata.cwd.clone(),
        provider: parent_metadata.provider.clone(),
        model: parent_metadata.model.clone(),
        max_tokens: parent_metadata.max_tokens,
        max_tokens_source: parent_metadata.max_tokens_source,
        permission_overrides: parent_metadata.permission_overrides,
        effort: parent_metadata.effort.clone(),
        fast: parent_metadata.fast,
        cache_policy: parent_metadata.cache_policy,
        system_prompt_version: "test-system-v1".into(),
        event_id: EventId::new("created-derived-child"),
        device_id: DeviceId::new("daemon-test"),
    };
    must(store.create_session_with_workspace_configuration(
        &command,
        mode,
        None,
        None,
        Some(parent_metadata.clone()),
    ));
    assert!(must(store.latest_launch_origin(&child)).is_none());
    must(store.register_session_launch_origin(&origin_command(
        &store,
        "created-child-origin",
        &child,
        "child-open",
        0,
        home_path("~/child"),
    )));
    assert_eq!(
        must(store.latest_launch_origin(&child))
            .expect("child")
            .revision,
        1
    );
    assert_eq!(
        must(store.session_metadata(&child))
            .expect("typed child")
            .launch_origin
            .expect("owned projection")
            .subject_session_id,
        child.as_str()
    );
    assert_eq!(must(store.session_metadata(&parent)), Some(parent_metadata));
}

#[test]
fn origin_child_creation_inherits_route_without_parent_open() {
    derived_create_origin(haider_protocol::session::SessionInteractionModeV1::Interactive);
}

#[test]
fn origin_delegated_creation_inherits_route_without_parent_open() {
    derived_create_origin(haider_protocol::session::SessionInteractionModeV1::Autonomous);
}

/// O2/O3: one registration per open — committed once, an identical retry
/// replays the exact receipt, and NO second event is appended by replay.
#[test]
fn origin_registers_once_and_identical_retry_replays_receipt() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-origin-once");
    seed_legacy_session(&store, &session);
    let seq_before = must(store.latest_seq(&session));
    let activity_before = must(store.session_recencies(std::slice::from_ref(&session)))[0]
        .key
        .last_activity_ms;

    let command = origin_command(
        &store,
        "origin-1",
        &session,
        "open-a",
        0,
        home_path("~/dev"),
    );
    let first = match must(store.register_session_launch_origin(&command)) {
        SessionLaunchOriginOutcome::Committed { origin, envelope } => {
            let fact =
                SessionLaunchOriginSelected::from_payload_value(&envelope.payload.to_json_value())
                    .expect("committed envelope is a launch-origin fact");
            assert_eq!(fact.revision, 1);
            assert_eq!(fact.open_id, "open-a");
            assert_eq!(fact.subject_session_id, session.as_str());
            assert_eq!(fact.workspace_materialized, Some(false));
            origin
        }
        other => panic!("expected committed, got {other:?}"),
    };
    assert_eq!(first.revision, 1);
    assert_eq!(first.path.display.as_deref(), Some("~/dev"));
    assert_eq!(must(store.latest_seq(&session)), seq_before + 1);
    assert_eq!(
        must(store.session_recencies(std::slice::from_ref(&session)))[0]
            .key
            .last_activity_ms,
        activity_before,
        "origin config must not advance roster activity"
    );

    let replay = match must(store.register_session_launch_origin(&command)) {
        SessionLaunchOriginOutcome::IdempotentReplay { origin } => origin,
        other => panic!("expected idempotent replay, got {other:?}"),
    };
    assert_eq!(replay, first, "replay must return the original snapshot");
    assert_eq!(
        must(store.latest_seq(&session)),
        seq_before + 1,
        "replay must append nothing"
    );
    // Ten repeated retries still no-op: once per session per open.
    for _ in 0..10 {
        must(store.register_session_launch_origin(&command));
    }
    assert_eq!(must(store.latest_seq(&session)), seq_before + 1);
}

/// O3: replacement is revision-CAS'd — a stale expected revision is a
/// conflict, never latest-timestamp-wins; an accepted replacement advances
/// the revision while raw history keeps every accepted registration.
#[test]
fn origin_replacement_is_cas_guarded_and_history_immutable() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-origin-cas");
    seed_legacy_session(&store, &session);

    let first = origin_command(
        &store,
        "origin-a",
        &session,
        "open-a",
        0,
        home_path("~/one"),
    );
    must(store.register_session_launch_origin(&first));

    // A second open that still believes revision 0 must conflict.
    let stale = origin_command(
        &store,
        "origin-b",
        &session,
        "open-b",
        0,
        home_path("~/two"),
    );
    let error = store
        .register_session_launch_origin(&stale)
        .expect_err("stale CAS must conflict");
    assert_eq!(error.code, ErrorCode::RevisionConflict);

    // The deliberate reopen with the CURRENT revision replaces the slot.
    let fresh = origin_command(
        &store,
        "origin-c",
        &session,
        "open-b",
        1,
        home_path("~/two"),
    );
    let replaced = match must(store.register_session_launch_origin(&fresh)) {
        SessionLaunchOriginOutcome::Committed { origin, .. } => origin,
        other => panic!("expected committed replacement, got {other:?}"),
    };
    assert_eq!(replaced.revision, 2);
    assert_eq!(replaced.open_id, "open-b");

    // Current snapshot is the replacement; the superseded registration
    // remains in the immutable journal (event count 2).
    let latest = must(store.latest_launch_origin(&session)).expect("origin snapshot");
    assert_eq!(latest.revision, 2);
    assert_eq!(latest.path.display.as_deref(), Some("~/two"));
    let connection = must(rusqlite::Connection::open(store.database_path()));
    let events: i64 = must(connection.query_row(
        "SELECT COUNT(*) FROM events WHERE session_id = ?1 \
         AND payload_kind = 'session_launch_origin_selected'",
        [session.as_str()],
        |row| row.get(0),
    ));
    assert_eq!(events, 2, "replaced registrations stay in raw history");

    // A replayed superseded command returns ITS original response but does
    // not resurrect the old origin as current (O2).
    let replayed = match must(store.register_session_launch_origin(&first)) {
        SessionLaunchOriginOutcome::IdempotentReplay { origin } => origin,
        other => panic!("expected replay, got {other:?}"),
    };
    assert_eq!(replayed.revision, 1);
    let latest = must(store.latest_launch_origin(&session)).expect("origin snapshot");
    assert_eq!(
        latest.revision, 2,
        "receipt replay must not restore old origin"
    );
}

/// O4: legacy `{}` metadata stays `{}`; the event alone carries origin and
/// the snapshot reconstructs from it.
#[test]
fn legacy_rows_stay_untyped_and_reconstruct_from_events() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-origin-legacy");
    seed_legacy_session(&store, &session);
    assert!(must(store.latest_launch_origin(&session)).is_none());

    let command = origin_command(&store, "origin-l", &session, "open-l", 0, home_path("~"));
    must(store.register_session_launch_origin(&command));

    // Typed metadata decode still returns none for `{}` rows.
    assert!(must(store.session_metadata(&session)).is_none());
    let latest = must(store.latest_launch_origin(&session)).expect("event reconstruction");
    assert_eq!(latest.revision, 1);
    assert_eq!(latest.open_id, "open-l");
}

/// O4: typed sessions get the transactional metadata projection, and a
/// concurrent-style workspace change does not lose the origin field.
#[test]
fn typed_projection_updates_and_survives_workspace_set() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let initial_workspace = root.path().join("initial-workspace");
    let session = typed_session(&store, "session-origin-typed", &initial_workspace);

    let command = origin_command(&store, "origin-t", &session, "open-t", 0, home_path("~/x"));
    must(store.register_session_launch_origin(&command));
    let metadata = must(store.session_metadata(&session)).expect("typed metadata");
    let origin = metadata.launch_origin.expect("projected origin");
    assert_eq!(origin.revision, 1);
    assert_eq!(origin.subject_session_id, session.as_str());

    // A workspace change (a different session-config mutation) preserves it.
    let replacement_workspace = root.path().join("replacement-workspace");
    let workspace = haider_store::SessionWorkspaceSetCommand {
        command_id: "workspace-1".into(),
        request_digest: "workspace-digest".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        path: replacement_workspace.to_string_lossy().into_owned(),
        event_id: EventId::new("workspace-evt"),
        device_id: DeviceId::new("device-test"),
    };
    must(store.set_session_workspace(&workspace));
    let metadata = must(store.session_metadata(&session)).expect("typed metadata");
    assert_eq!(metadata.cwd, replacement_workspace.to_string_lossy());
    assert_eq!(
        metadata.launch_origin.expect("origin survives").revision,
        1,
        "workspace set must not clobber the origin projection"
    );
    // And workspace set emitted no origin event (O7: workspace authority
    // changes only).
    let connection = must(rusqlite::Connection::open(store.database_path()));
    let events: i64 = must(connection.query_row(
        "SELECT COUNT(*) FROM events WHERE session_id = ?1 \
         AND payload_kind = 'session_launch_origin_selected'",
        [session.as_str()],
        |row| row.get(0),
    ));
    assert_eq!(events, 1);
}

/// O6: ingress revalidation — unescaped control text, over-limit displays,
/// and a missing display for a non-unavailable kind are refused without
/// claiming the receipt (the same command id can then register cleanly).
#[test]
fn unsanitised_ingress_is_refused_without_claiming_the_receipt() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-origin-dirty");
    seed_legacy_session(&store, &session);

    let bidi_path = root.path().join("a\u{202e}b");
    let newline_path = root.path().join("a\nb");
    let over_limit_path = root.path().join("x".repeat(4096));
    for path in [
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some(bidi_path.to_string_lossy().into_owned()),
        },
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some(newline_path.to_string_lossy().into_owned()),
        },
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some(over_limit_path.to_string_lossy().into_owned()),
        },
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::HomeRelative,
            display: None,
        },
    ] {
        let command = origin_command(&store, "origin-dirty", &session, "open-d", 0, path);
        let error = store
            .register_session_launch_origin(&command)
            .expect_err("unsanitised origin must be refused");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    assert!(must(store.latest_launch_origin(&session)).is_none());

    // The refused command id was never claimed; a clean registration with
    // the same id succeeds (no corrupted session, no stuck receipt).
    let clean = origin_command(
        &store,
        "origin-dirty",
        &session,
        "open-d",
        0,
        home_path("~"),
    );
    let outcome = must(store.register_session_launch_origin(&clean));
    assert!(matches!(
        outcome,
        SessionLaunchOriginOutcome::Committed { .. }
    ));

    // The unavailable kind legitimately has no display.
    let unavailable = origin_command(
        &store,
        "origin-unavailable",
        &session,
        "open-e",
        1,
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Unavailable,
            display: None,
        },
    );
    must(store.register_session_launch_origin(&unavailable));
}

/// A stale worker generation is refused before any mutation.
#[test]
fn stale_generation_is_refused() {
    let root = test_root();
    let store = must(Store::open(root.path()));
    let session = SessionId::new("session-origin-stale");
    seed_legacy_session(&store, &session);
    let mut command = origin_command(&store, "origin-s", &session, "open-s", 0, home_path("~"));
    command.worker_generation += 1;
    let error = store
        .register_session_launch_origin(&command)
        .expect_err("stale generation must be refused");
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);
    assert!(must(store.latest_launch_origin(&session)).is_none());
}

/// F1: list/observe metadata must expose exactly the origin used by attach/CAS.
/// The own journal survives absent, foreign, and pre-epoch projections.
#[test]
fn recovered_own_origin_is_visible_to_fresh_registration() {
    for projection in ["missing", "foreign", "pre-epoch"] {
        let root = test_root();
        let store = must(Store::open(root.path()));
        let parent = typed_session(&store, "recovery-parent", root.path());
        let child = typed_session(&store, "recovery-child", root.path());
        must(store.register_session_launch_origin(&origin_command(
            &store,
            "parent",
            &parent,
            "parent-open",
            0,
            home_path("~/parent"),
        )));
        for revision in 0..4 {
            must(store.register_session_launch_origin(&origin_command(
                &store,
                &format!("child-{revision}"),
                &child,
                &format!("child-open-{revision}"),
                revision,
                home_path("~/child"),
            )));
        }
        let own = must(store.latest_launch_origin(&child)).expect("own journal origin");
        let foreign = must(store.latest_launch_origin(&parent)).expect("foreign origin");
        let connection = must(rusqlite::Connection::open(store.database_path()));
        if projection == "foreign" {
            must(connection.execute(
                "UPDATE sessions SET meta_json=json_set(meta_json,'$.launch_origin',json(?1)) WHERE id=?2",
                rusqlite::params![must(serde_json::to_string(&foreign)),child.as_str()],
            ));
        } else {
            must(connection.execute(
                "UPDATE sessions SET meta_json=json_remove(meta_json,'$.launch_origin') WHERE id=?1",
                [child.as_str()],
            ));
        }
        if projection == "pre-epoch" {
            must(connection.execute(
                "UPDATE sessions SET meta_json=json_remove(meta_json,'$.selection_epoch') WHERE id=?1",
                [child.as_str()],
            ));
        }
        drop(connection);
        drop(store);
        let store = must(Store::open(root.path()));
        let metadata = must(store.session_metadata(&child)).expect("typed session");
        assert_eq!(metadata.launch_origin, Some(own.clone()), "{projection}");
        assert_eq!(must(store.latest_launch_origin(&child)), Some(own));
        let stale = store
            .register_session_launch_origin(&origin_command(
                &store,
                "stale",
                &child,
                "stale-open",
                0,
                home_path("~/stale"),
            ))
            .expect_err("a real stale revision must remain refused");
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        must(store.register_session_launch_origin(&origin_command(
            &store,
            "fresh",
            &child,
            "fresh-open",
            metadata.launch_origin.expect("recovered revision").revision,
            home_path("~/fresh"),
        )));
        assert_eq!(
            must(store.latest_launch_origin(&child))
                .expect("new origin")
                .revision,
            5
        );
        assert_eq!(must(store.latest_launch_origin(&parent)), Some(foreign));
    }
}
