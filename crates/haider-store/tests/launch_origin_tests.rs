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
    EventStore, SessionCreateCommand, SessionLaunchOriginCommand, SessionLaunchOriginOutcome,
    Store,
};
use serde_json::json;
use std::fmt::Debug;

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
    let request_json = format!(
        "{{\"open_id\":{open_id:?},\"expected_revision\":{expected_revision}}}"
    );
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

fn typed_session(store: &Store, session_id: &str) -> SessionId {
    let command = SessionCreateCommand {
        command_id: format!("create-{session_id}"),
        request_digest: "digest-create".into(),
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
        event_id: EventId::new(format!("created-{session_id}")),
        device_id: DeviceId::new("daemon-test"),
    };
    must(store.create_session(&command));
    SessionId::new(session_id)
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

    let command = origin_command(&store, "origin-1", &session, "open-a", 0, home_path("~/dev"));
    let first = match must(store.register_session_launch_origin(&command)) {
        SessionLaunchOriginOutcome::Committed { origin, envelope } => {
            let fact = SessionLaunchOriginSelected::from_payload_value(
                &envelope.payload.to_json_value(),
            )
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

    let first = origin_command(&store, "origin-a", &session, "open-a", 0, home_path("~/one"));
    must(store.register_session_launch_origin(&first));

    // A second open that still believes revision 0 must conflict.
    let stale = origin_command(&store, "origin-b", &session, "open-b", 0, home_path("~/two"));
    let error = store
        .register_session_launch_origin(&stale)
        .expect_err("stale CAS must conflict");
    assert_eq!(error.code, ErrorCode::RevisionConflict);

    // The deliberate reopen with the CURRENT revision replaces the slot.
    let fresh = origin_command(&store, "origin-c", &session, "open-b", 1, home_path("~/two"));
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
    assert_eq!(latest.revision, 2, "receipt replay must not restore old origin");
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
    let session = typed_session(&store, "session-origin-typed");

    let command = origin_command(&store, "origin-t", &session, "open-t", 0, home_path("~/x"));
    must(store.register_session_launch_origin(&command));
    let metadata = must(store.session_metadata(&session)).expect("typed metadata");
    let origin = metadata.launch_origin.expect("projected origin");
    assert_eq!(origin.revision, 1);
    assert_eq!(origin.subject_session_id, session.as_str());

    // A workspace change (a different session-config mutation) preserves it.
    let workspace = haider_store::SessionWorkspaceSetCommand {
        command_id: "workspace-1".into(),
        request_digest: "workspace-digest".into(),
        request_json: "{}".into(),
        session_id: session.clone(),
        worker_generation: store.worker_generation(),
        path: "/tmp".into(),
        event_id: EventId::new("workspace-evt"),
        device_id: DeviceId::new("device-test"),
    };
    must(store.set_session_workspace(&workspace));
    let metadata = must(store.session_metadata(&session)).expect("typed metadata");
    assert_eq!(metadata.cwd, "/tmp");
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

    for path in [
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some("/opt/a\u{202e}b".into()),
        },
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some("/opt/a\nb".into()),
        },
        LaunchOriginPathV1 {
            kind: LaunchOriginPathKindV1::Absolute,
            display: Some(format!("/opt/{}", "x".repeat(4096))),
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
    let clean = origin_command(&store, "origin-dirty", &session, "open-d", 0, home_path("~"));
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
    let mut command =
        origin_command(&store, "origin-s", &session, "open-s", 0, home_path("~"));
    command.worker_generation += 1;
    let error = store
        .register_session_launch_origin(&command)
        .expect_err("stale generation must be refused");
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);
    assert!(must(store.latest_launch_origin(&session)).is_none());
}
