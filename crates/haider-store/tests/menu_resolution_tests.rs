//! Durable MenuAnswer compare-and-set acceptance tests.

#![allow(clippy::expect_used)]

use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::ids::{DeviceId, EventId, MenuId, RunId, SessionId};
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_store::{
    EventStore, MenuResolutionCommand, MenuResolutionOutcome, SessionCreateCommand, Store,
    TurnAcceptCommand,
};
use std::sync::{Arc, Barrier};

fn envelope(
    session_id: &SessionId,
    event_id: &str,
    worker_generation: u64,
    payload: EventPayload,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("menu-test"),
        authority_epoch: 7,
        worker_generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload serializes"),
    }
}

fn menu(menu_id: &MenuId) -> Menu {
    Menu {
        id: menu_id.clone(),
        kind: MenuKind::Choice,
        title: "Choose".into(),
        body: Vec::new(),
        options: vec![
            MenuOption {
                key: "allow".into(),
                label: "Allow".into(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "deny".into(),
                label: "Deny".into(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "test".into(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn seed_menu(store: &Store, session_id: &SessionId, menu_id: &MenuId) -> u64 {
    let mut opening = vec![envelope(
        session_id,
        "menu-opened",
        store.worker_generation(),
        EventPayload::MenuOpened(menu(menu_id)),
    )];
    store.append(&mut opening).expect("menu opening commits");
    opening[0].seq
}

fn command(
    store: &Store,
    session_id: &SessionId,
    menu_id: &MenuId,
    request_seq: u64,
    command_id: &str,
    option_index: u32,
    option_key: &str,
) -> MenuResolutionCommand {
    MenuResolutionCommand {
        command_id: command_id.into(),
        session_id: session_id.clone(),
        request_seq,
        worker_generation: store.worker_generation(),
        allow_prior_generation: false,
        answer: MenuAnswer {
            menu: menu_id.clone(),
            option_key: Some(option_key.into()),
            option_index,
            value: None,
            via: AnswerVia::Rpc,
        },
        device_id: DeviceId::new("menu-controller"),
        input_is_secret_reference: false,
    }
}

fn create_typed_session(store: &Store, session_id: &SessionId) {
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
            system_prompt_version: "test-system-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("create session");
}

fn turn_command(store: &Store, session_id: &SessionId, command_id: &str) -> TurnAcceptCommand {
    TurnAcceptCommand {
        command_id: command_id.into(),
        request_digest: "turn-digest".into(),
        request_json: r#"{"text":"global namespace"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(format!("run-{command_id}")),
        agent_id: None,
        text: "global namespace".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{command_id}")),
        user_event_id: EventId::new(format!("user-{command_id}")),
        active_event_id: EventId::new(format!("active-{command_id}")),
        device_id: DeviceId::new("test-daemon"),
    }
}

/// MUTATION CHECK: skip both indexed and historical existing-resolution
/// lookups. Expected failure: losing contenders receive a uniqueness error
/// instead of the winner's durable resolution coordinate.
/// Verified by revert on 2026-07-27.
#[test]
fn n_way_race_commits_exactly_one_answer_and_returns_one_resolution_coordinate() {
    let root = tempfile::tempdir().expect("temp store");
    let store = Arc::new(Store::open(root.path()).expect("open store"));
    let session_id = SessionId::new("race-session");
    let menu_id = MenuId::new("race-menu");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    let contenders = 12;
    let barrier = Arc::new(Barrier::new(contenders));

    let tasks = (0..contenders)
        .map(|index| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let menu_id = menu_id.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let command = command(
                    &store,
                    &session_id,
                    &menu_id,
                    request_seq,
                    &format!("command-{index}"),
                    0,
                    "allow",
                );
                barrier.wait();
                store.resolve_menu(&command).expect("CAS completes")
            })
        })
        .collect::<Vec<_>>();
    let outcomes = tasks
        .into_iter()
        .map(|task| task.join().expect("contender joins"))
        .collect::<Vec<_>>();

    let winners = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            MenuResolutionOutcome::Committed { envelope } => Some(envelope.seq),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1);
    let winner_seq = winners[0];
    assert!(outcomes.iter().all(|outcome| match outcome {
        MenuResolutionOutcome::Committed { envelope } => envelope.seq == winner_seq,
        MenuResolutionOutcome::AlreadyResolved { resolution_seq } => {
            *resolution_seq == winner_seq
        }
        MenuResolutionOutcome::IdempotentReplay { .. } => false,
    }));
    let history = store.journal_replay(&session_id).expect("history replays");
    assert_eq!(
        history
            .iter()
            .filter(|envelope| {
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::MenuAnswered(_)))
            })
            .count(),
        1
    );
}

/// MUTATION CHECK: move the generation check after the winner lookup/append.
/// Expected failure: a stale command advances an unresolved head or receives
/// an `AlreadyResolved` coordinate for a generation it is not allowed to use.
#[test]
fn stale_generation_is_fenced_without_advancing_the_head() {
    let root = tempfile::tempdir().expect("temp store");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("stale-session");
    let menu_id = MenuId::new("stale-menu");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    let head = store.latest_seq(&session_id).expect("head");
    let mut stale = command(
        &store,
        &session_id,
        &menu_id,
        request_seq,
        "stale-command",
        0,
        "allow",
    );
    stale.worker_generation = stale.worker_generation.saturating_sub(1);

    let error = store
        .resolve_menu(&stale)
        .expect_err("stale command rejects");
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);
    assert_eq!(store.latest_seq(&session_id).expect("head"), head);

    let winner = command(
        &store,
        &session_id,
        &menu_id,
        request_seq,
        "winner-command",
        0,
        "allow",
    );
    let _ = store.resolve_menu(&winner).expect("winner commits");
    let resolved_head = store.latest_seq(&session_id).expect("resolved head");
    let mut stale_loser = command(
        &store,
        &session_id,
        &menu_id,
        request_seq,
        "stale-loser",
        0,
        "deny",
    );
    stale_loser.worker_generation = stale_loser.worker_generation.saturating_sub(1);
    let error = store
        .resolve_menu(&stale_loser)
        .expect_err("resolved menu does not bypass the generation fence");
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);
    assert_eq!(store.latest_seq(&session_id).expect("head"), resolved_head);
}

/// MUTATION CHECK: omit the command-id lookup before generation fencing.
/// Expected failure: after reopen the lost-response retry is rejected stale
/// instead of returning the original committed resolution sequence.
#[test]
fn lost_response_retry_after_reopen_is_idempotent_and_never_reappends() {
    let root = tempfile::tempdir().expect("temp store");
    let session_id = SessionId::new("retry-session");
    let menu_id = MenuId::new("retry-menu");
    let (command, resolution_seq) = {
        let store = Store::open(root.path()).expect("open store");
        let request_seq = seed_menu(&store, &session_id, &menu_id);
        let command = command(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "durable-command",
            0,
            "allow",
        );
        let MenuResolutionOutcome::Committed { envelope } =
            store.resolve_menu(&command).expect("first answer commits")
        else {
            panic!("first answer did not commit");
        };
        (command, envelope.seq)
    };

    let reopened = Store::open(root.path()).expect("reopen store");
    assert_eq!(
        reopened.resolve_menu(&command).expect("retry resolves"),
        MenuResolutionOutcome::IdempotentReplay { resolution_seq }
    );
    assert_eq!(
        reopened.journal_replay(&session_id).expect("history").len(),
        2,
        "restart/retry must not append or resend the protected resolution"
    );
}

#[test]
fn menu_answers_and_rpc_commands_share_one_global_command_id_namespace() {
    let root = tempfile::tempdir().expect("temp store");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("menu-first-global-command");
    create_typed_session(&store, &session_id);
    let menu_id = MenuId::new("menu-first");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    store
        .resolve_menu(&command(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "global-command",
            0,
            "allow",
        ))
        .expect("menu answer commits");
    let error = store
        .accept_turn(&turn_command(&store, &session_id, "global-command"))
        .expect_err("turn cannot reuse menu command id");
    assert_eq!(error.code, ErrorCode::InvalidArgument);

    let root = tempfile::tempdir().expect("second temp store");
    let store = Store::open(root.path()).expect("second store");
    let session_id = SessionId::new("turn-first-global-command");
    create_typed_session(&store, &session_id);
    store
        .accept_turn(&turn_command(&store, &session_id, "global-command"))
        .expect("turn commits");
    let menu_id = MenuId::new("turn-first");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    let error = store
        .resolve_menu(&command(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "global-command",
            0,
            "allow",
        ))
        .expect_err("menu cannot reuse turn command id");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

/// MUTATION CHECK: delete the post-opening journal scan. Expected failure: a
/// historical `MenuAnswered` without an index row accepts a second answer.
#[test]
fn historical_answer_without_resolution_index_still_fences_a_second_answer() {
    let root = tempfile::tempdir().expect("temp store");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("historical-session");
    let menu_id = MenuId::new("historical-menu");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    let historical_answer = MenuAnswer {
        menu: menu_id.clone(),
        option_key: Some("deny".into()),
        option_index: 1,
        value: None,
        via: AnswerVia::Rpc,
    };
    let mut historical = vec![envelope(
        &session_id,
        "historical-answer",
        store.worker_generation(),
        EventPayload::MenuAnswered(historical_answer),
    )];
    store
        .append(&mut historical)
        .expect("historical answer commits");

    let outcome = store
        .resolve_menu(&command(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "late-command",
            0,
            "allow",
        ))
        .expect("CAS inspects history");
    assert_eq!(
        outcome,
        MenuResolutionOutcome::AlreadyResolved {
            resolution_seq: historical[0].seq
        }
    );
}

/// MUTATION CHECK: validate only the option key or only the option index.
/// Expected failure: the mixed-version answer commits and advances the head.
#[test]
fn option_key_and_index_must_match_the_committed_menu_version() {
    let root = tempfile::tempdir().expect("temp store");
    let store = Store::open(root.path()).expect("open store");
    let session_id = SessionId::new("version-session");
    let menu_id = MenuId::new("version-menu");
    let request_seq = seed_menu(&store, &session_id, &menu_id);
    let head = store.latest_seq(&session_id).expect("head");

    let error = store
        .resolve_menu(&command(
            &store,
            &session_id,
            &menu_id,
            request_seq,
            "mixed-command",
            1,
            "allow",
        ))
        .expect_err("mixed key/index rejects");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(store.latest_seq(&session_id).expect("head"), head);
}
