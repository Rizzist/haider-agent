#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::state::{RunState, SessionState};
use haider_store::{
    EventStore, SessionCreateCommand, Store, TurnAcceptCommand, TurnAcceptOutcome,
    TurnAdmissionDisposition, TurnCancelCommand, TurnCancelOutcome, TurnCancellationStatus,
};
use std::sync::{Arc, Barrier};

fn create(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: "create-digest".into(),
            request_json: r#"{"cwd":"/tmp","max_tokens":4096,"model":"fake-v1","provider":"fake"}"#
                .into(),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-v1".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            system_prompt_version: "test-system-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("create session");
}

fn submit(
    store: &Store,
    command_id: &str,
    session_id: &SessionId,
    run_id: &str,
) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: command_id.into(),
        request_digest: "submit-digest".into(),
        request_json: format!(
            r#"{{"attachments":[],"mode":"queue","session_id":"{session_id}","text":"hello","worker_generation":{}}}"#,
            store.worker_generation()
        ),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(run_id),
        agent_id: None,
        branch_id: None,
        text: "hello".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{run_id}")),
        user_event_id: EventId::new(format!("user-{run_id}")),
        active_event_id: EventId::new(format!("active-{run_id}")),
        device_id: DeviceId::new("test-daemon"),
    }
}

/// MUTATION CHECK: split receipt/Queued/UserMessage/ActiveRun across
/// transactions. Expected failure: the returned accepted sequence or exact
/// durable acceptance/tree prefix is incomplete.
#[test]
fn submit_atomically_commits_receipt_and_runnable_prefix() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-a");
    create(&store, &session_id);
    let outcome = store
        .accept_turn(&submit(&store, "submit-a", &session_id, "run-a"))
        .expect("accept");
    let TurnAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = outcome
    else {
        panic!("first submit commits");
    };
    assert_eq!(accepted.accepted_seq, 3);
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    assert_eq!(
        envelopes.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(store.latest_seq(&session_id).expect("head"), 5);
}

/// MUTATION CHECK: queue a fresh run or append a second `Queued` prefix for a
/// same-run steer. Expected runtime failure: disposition is not
/// `SteerPending`, more than one UserMessage is appended, or its atomic tree
/// node sidecar is absent.
#[test]
fn same_run_steer_commits_one_message_without_minting_a_new_turn() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-steer");
    create(&store, &session_id);
    store
        .accept_turn(&submit(
            &store,
            "submit-steer-base",
            &session_id,
            "run-steer",
        ))
        .expect("base turn");

    let command = TurnAcceptCommand {
        command_id: "submit-steer-nudge".into(),
        request_digest: "submit-steer-nudge-digest".into(),
        request_json: r#"{"run_id":"run-steer","text":"report your status or conclude"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("run-steer"),
        agent_id: None,
        branch_id: None,
        text: "report your status or conclude".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Steer,
        queued_event_id: EventId::new("unused-steer-queued"),
        user_event_id: EventId::new("steer-user"),
        active_event_id: EventId::new("unused-steer-active"),
        device_id: DeviceId::new("test-daemon"),
    };
    let outcome = store.accept_turn(&command).expect("same-run steer");
    let TurnAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = outcome
    else {
        panic!("first steer commits");
    };
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::SteerPending);
    assert_eq!(accepted.run_id, RunId::new("run-steer"));
    assert_eq!(envelopes.len(), 2);
    assert_eq!(accepted.accepted_seq, envelopes[0].seq);
    assert!(matches!(
        serde_json::from_value::<haider_protocol::EventPayload>(envelopes[0].payload.clone())
            .expect("payload"),
        haider_protocol::EventPayload::UserMessage { text, mode, .. }
            if text == "report your status or conclude" && mode == DeliveryMode::Steer
    ));
    assert!(matches!(
        serde_json::from_value::<haider_protocol::EventPayload>(envelopes[1].payload.clone())
            .expect("tree payload"),
        haider_protocol::EventPayload::NodeCommitted(node)
            if matches!(node.kind, haider_protocol::history::NodeKind::UserTurn { ref text, .. }
                if text == "report your status or conclude")
    ));
    assert!(matches!(
        store.accept_turn(&command).expect("steer replay"),
        TurnAcceptOutcome::IdempotentReplay { .. }
    ));
}

/// ST1 daemon-admission law. MUTATION CHECK: treat Subturn as Queue, fail to
/// bind the daemon-minted candidate to the active run, or reuse SteerPending.
/// Expected runtime failure: a Queued prefix appears or the disposition/run
/// id differs.
#[test]
fn subturn_binds_to_the_active_run_with_a_distinct_durable_disposition() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-subturn");
    create(&store, &session_id);
    store
        .accept_turn(&submit(
            &store,
            "submit-subturn-base",
            &session_id,
            "run-subturn-active",
        ))
        .expect("base turn");
    let mut streaming = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("subturn-streaming"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("run-subturn-active")),
        agent_id: None,
        device_id: DeviceId::new("test-daemon"),
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
        payload: serde_json::to_value(haider_protocol::EventPayload::RunState(RunState::Streaming))
            .expect("streaming payload"),
    }];
    store.append(&mut streaming).expect("streaming append");

    let command = TurnAcceptCommand {
        command_id: "submit-subturn-input".into(),
        request_digest: "submit-subturn-input-digest".into(),
        request_json: r#"{"mode":"subturn","text":"use narrow args"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("daemon-minted-subturn-candidate"),
        agent_id: None,
        branch_id: None,
        text: "use narrow args".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Subturn,
        queued_event_id: EventId::new("unused-subturn-queued"),
        user_event_id: EventId::new("subturn-user"),
        active_event_id: EventId::new("unused-subturn-active"),
        device_id: DeviceId::new("test-daemon"),
    };
    let TurnAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = store.accept_turn(&command).expect("subturn accepts")
    else {
        panic!("first subturn commits");
    };
    assert_eq!(accepted.run_id, RunId::new("run-subturn-active"));
    assert_eq!(
        accepted.disposition,
        TurnAdmissionDisposition::SubturnPending
    );
    assert_eq!(envelopes.len(), 2, "same-run delivery has no Queued prefix");
    assert!(matches!(
        serde_json::from_value::<haider_protocol::EventPayload>(envelopes[0].payload.clone())
            .expect("subturn payload"),
        haider_protocol::EventPayload::UserMessage { text, mode, .. }
            if text == "use narrow args" && mode == DeliveryMode::Subturn
    ));
}

#[test]
fn legacy_session_without_typed_metadata_is_rejected_before_acceptance() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("legacy-session");
    let mut legacy = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("legacy-created"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("legacy-cli"),
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
        payload: serde_json::to_value(haider_protocol::EventPayload::SessionState(
            SessionState::Idle { interrupted: false },
        ))
        .expect("payload"),
    }];
    store.append(&mut legacy).expect("legacy session seed");
    let command = submit(&store, "legacy-submit", &session_id, "legacy-run");
    let error = store
        .accept_turn(&command)
        .expect_err("acceptance guarantees supervisor metadata");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
    assert_eq!(store.latest_seq(&session_id).expect("head"), 1);
    assert!(
        store
            .turn_accept_receipt(
                "legacy-submit",
                &command.request_digest,
                &command.request_json,
            )
            .expect("receipt lookup")
            .is_none()
    );
}

/// MUTATION CHECK: compare only command id. Expected failure: the changed
/// semantic body below replays the first response instead of rejecting it.
#[test]
fn submit_replay_is_idempotent_and_changed_body_is_rejected() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-b");
    create(&store, &session_id);
    let first = store
        .accept_turn(&submit(&store, "submit-b", &session_id, "run-b"))
        .expect("first");
    let replay = store
        .accept_turn(&submit(&store, "submit-b", &session_id, "ignored-run"))
        .expect("replay");
    assert!(matches!(first, TurnAcceptOutcome::Committed { .. }));
    assert!(matches!(replay, TurnAcceptOutcome::IdempotentReplay { .. }));
    let mut changed = submit(&store, "submit-b", &session_id, "changed");
    changed.request_digest = "other".into();
    changed.request_json = r#"{"text":"other"}"#.into();
    assert!(store.accept_turn(&changed).is_err());
    assert_eq!(store.latest_seq(&session_id).expect("head"), 5);
}

/// MUTATION CHECK: look up the receipt before taking the IMMEDIATE write
/// transaction. Expected failure: both racing submissions can allocate runs.
#[test]
fn concurrent_submit_has_one_commit_and_one_replay() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(root.path()).expect("store"));
    let session_id = SessionId::new("session-c");
    create(&store, &session_id);
    let barrier = Arc::new(Barrier::new(3));
    let handles = ["run-c1", "run-c2"]
        .into_iter()
        .map(|run_id| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let command = submit(&store, "submit-c", &session_id, run_id);
                barrier.wait();
                store.accept_turn(&command).expect("racing accept")
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
            .filter(|outcome| matches!(outcome, TurnAcceptOutcome::Committed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, TurnAcceptOutcome::IdempotentReplay { .. }))
            .count(),
        1
    );
}

/// MUTATION CHECK: signal/cancel outside the cancellation transaction or
/// skip the terminal-state reduction. Expected failure: no Cancelling fact
/// is returned for the active run or a terminal run is cancelled again.
#[test]
fn cancel_records_intent_and_reports_already_terminal() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-d");
    create(&store, &session_id);
    store
        .accept_turn(&submit(&store, "submit-d", &session_id, "run-d"))
        .expect("submit");
    let cancel = |command_id: &str, event_id: &str| TurnCancelCommand {
        command_id: command_id.into(),
        request_digest: format!("digest-{command_id}"),
        request_json: format!(
            r#"{{"run_id":"run-d","session_id":"session-d","worker_generation":{}}}"#,
            store.worker_generation()
        ),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("run-d"),
        cancelling_event_id: EventId::new(event_id),
        device_id: DeviceId::new("test-daemon"),
    };
    let first = store
        .cancel_turn(&cancel("cancel-d", "cancelling-d"))
        .expect("cancel");
    assert!(matches!(
        first,
        TurnCancelOutcome::Committed {
            ref cancelled,
            envelope: Some(_),
        } if cancelled.status == TurnCancellationStatus::Accepted
    ));
    let head_after_first = store.latest_seq(&session_id).expect("head");
    let duplicate = store
        .cancel_turn(&cancel("cancel-d-duplicate", "unused-duplicate"))
        .expect("duplicate intent");
    assert!(matches!(
        duplicate,
        TurnCancelOutcome::Committed {
            ref cancelled,
            envelope: None,
        } if cancelled.status == TurnCancellationStatus::Accepted
    ));
    assert_eq!(
        store.latest_seq(&session_id).expect("head"),
        head_after_first,
        "latest Cancelling is not duplicated"
    );

    let mut terminal = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("cancelled-d"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("run-d")),
        agent_id: None,
        device_id: DeviceId::new("worker"),
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
        payload: serde_json::to_value(haider_protocol::EventPayload::RunState(RunState::Cancelled))
            .expect("payload"),
    }];
    store.append(&mut terminal).expect("terminal append");
    let terminal_seq = terminal[0].seq;
    let second = store
        .cancel_turn(&cancel("cancel-d-2", "unused"))
        .expect("terminal cancel");
    assert!(matches!(
        second,
        TurnCancelOutcome::Committed {
            ref cancelled,
            envelope: None,
        } if cancelled.status == TurnCancellationStatus::AlreadyTerminal
            && cancelled.terminal_seq == Some(terminal_seq)
    ));
}

/// The R2 receipt-idempotency law for `turn.cancel`, matching
/// `submit_replay_is_idempotent_and_changed_body_is_rejected` for
/// `turn.submit`.
///
/// MUTATION CHECK: compare only command id in the receipt lookup. Expected
/// failure: the changed semantic body below replays the first cancellation
/// response instead of rejecting it.
#[test]
fn cancel_replay_is_idempotent_and_changed_body_is_rejected() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-e");
    create(&store, &session_id);
    store
        .accept_turn(&submit(&store, "submit-e", &session_id, "run-e"))
        .expect("submit");
    let cancel = |digest: &str, json: &str, event_id: &str| TurnCancelCommand {
        command_id: "cancel-e".into(),
        request_digest: digest.into(),
        request_json: json.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("run-e"),
        cancelling_event_id: EventId::new(event_id),
        device_id: DeviceId::new("test-daemon"),
    };
    let original_json = format!(
        r#"{{"run_id":"run-e","session_id":"session-e","worker_generation":{}}}"#,
        store.worker_generation()
    );
    let first = store
        .cancel_turn(&cancel("digest-e", &original_json, "cancelling-e"))
        .expect("first cancel");
    assert!(matches!(first, TurnCancelOutcome::Committed { .. }));
    let head_after_first = store.latest_seq(&session_id).expect("head");
    let replay = store
        .cancel_turn(&cancel("digest-e", &original_json, "cancelling-e-replay"))
        .expect("replay");
    assert!(matches!(replay, TurnCancelOutcome::IdempotentReplay { .. }));
    assert!(
        store
            .cancel_turn(&cancel(
                "digest-other",
                r#"{"run_id":"other"}"#,
                "cancelling-e-changed",
            ))
            .is_err()
    );
    // Neither the replay nor the rejected reuse appended anything.
    assert_eq!(
        store.latest_seq(&session_id).expect("head"),
        head_after_first
    );
}

/// MUTATION CHECK: replace `settle_session_idle` with an ordinary append.
/// Expected failure: the stale worker observation commits Idle after the
/// concurrently accepted run's ActiveRun.
/// Verified by revert on 2026-07-27.
#[test]
fn aggregate_idle_is_skipped_when_a_new_run_is_durably_active() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("session-idle-race");
    create(&store, &session_id);
    store
        .accept_turn(&submit(&store, "submit-idle-a", &session_id, "run-idle-a"))
        .expect("first accept");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("done-idle-a"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(RunId::new("run-idle-a")),
        agent_id: None,
        device_id: DeviceId::new("worker"),
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
        payload: serde_json::to_value(haider_protocol::EventPayload::RunState(RunState::Done))
            .expect("payload"),
    }];
    store.append(&mut done).expect("first terminal");
    store
        .accept_turn(&submit(&store, "submit-idle-b", &session_id, "run-idle-b"))
        .expect("concurrent accept");

    let mut idle = EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("stale-idle"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("worker"),
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
        payload: serde_json::to_value(haider_protocol::EventPayload::SessionState(
            SessionState::Idle { interrupted: false },
        ))
        .expect("payload"),
    };
    assert!(!store.settle_session_idle(&mut idle).expect("settle"));
    assert_eq!(idle.seq, 0);
    let durable = store.read(&session_id, 0, 64).expect("read");
    assert!(matches!(
        durable.last().and_then(|envelope| {
            serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload.clone()).ok()
        }),
        Some(haider_protocol::EventPayload::SessionState(
            SessionState::ActiveRun
        ))
    ));
}
