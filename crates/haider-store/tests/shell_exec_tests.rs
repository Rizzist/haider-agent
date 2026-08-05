#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::{RunState, SessionState};
use haider_store::{
    EventStore, SessionCreateCommand, ShellExecAcceptCommand, ShellExecAcceptOutcome, Store,
};

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

fn command(store: &Store, session_id: &SessionId, body: &str) -> ShellExecAcceptCommand {
    ShellExecAcceptCommand {
        command_id: "shell-command-1".into(),
        request_digest: format!("digest-{body}"),
        request_json: format!(r#"{{"command":{body:?}}}"#),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new("shell-run-1"),
        item_id: ItemId::new("shell-item-1"),
        command: body.into(),
        running_event_id: EventId::new("shell-running-1"),
        item_event_id: EventId::new("shell-item-started-1"),
        active_event_id: EventId::new("shell-active-1"),
        device_id: DeviceId::new("test-daemon"),
    }
}

/// MUTATION CHECK: split the receipt from the synthetic run/item prefix or
/// append a UserMessage. Expected runtime failure: the exact three-envelope
/// runtime prefix, accepted sequence, or provider-free payload assertion
/// differs.
#[test]
fn shell_acceptance_atomically_commits_receipt_without_user_message() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("shell-session");
    create(&store, &session_id);
    let command = command(&store, &session_id, "printf exact");

    let ShellExecAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = store.accept_shell_exec(&command).expect("accept shell")
    else {
        panic!("first command must commit");
    };
    assert_eq!(accepted.accepted_seq, 3);
    assert_eq!(
        envelopes.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [2, 3, 4]
    );
    let payloads = envelopes
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        payloads[0],
        EventPayload::RunState(RunState::RunningTool)
    ));
    assert!(matches!(
        &payloads[1],
        EventPayload::Item(ItemEvent::Started {
            item_id,
            item: TurnItem::CommandExecution {
                call_id,
                command,
                status: ToolStatus::InProgress,
                exit_code: None,
            },
        }) if item_id == &ItemId::new("shell-item-1")
            && call_id == "shell-command-1"
            && command == "printf exact"
    ));
    assert!(matches!(
        payloads[2],
        EventPayload::SessionState(SessionState::ActiveRun)
    ));
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::UserMessage { .. }))
    );

    assert!(matches!(
        store.accept_shell_exec(&command).expect("same-id replay"),
        ShellExecAcceptOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(store.latest_seq(&session_id).expect("head"), 4);
}

/// MUTATION CHECK: key receipt replay only by command id and ignore the
/// canonical request digest/body. Expected runtime failure: changed command
/// bytes replay successfully instead of returning InvalidArgument at runtime.
#[test]
fn shell_receipt_rejects_changed_bytes_under_the_same_command_id() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("shell-mismatch");
    create(&store, &session_id);
    store
        .accept_shell_exec(&command(&store, &session_id, "printf first"))
        .expect("first command");

    let error = store
        .shell_exec_receipt(
            "shell-command-1",
            "digest-printf second",
            r#"{"command":"printf second"}"#,
        )
        .expect_err("changed bytes must not replay");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
}

/// MUTATION CHECK: remove the nonterminal-run check before receipt claim.
/// Expected runtime failure: a second direct shell command is committed while
/// the first synthetic run still owns the session.
#[test]
fn shell_acceptance_is_typed_busy_while_a_run_is_active() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("shell-busy");
    create(&store, &session_id);
    store
        .accept_shell_exec(&command(&store, &session_id, "printf first"))
        .expect("first command");
    let mut second = command(&store, &session_id, "printf second");
    second.command_id = "shell-command-2".into();
    second.request_digest = "digest-second".into();
    second.request_json = r#"{"command":"printf second"}"#.into();
    second.run_id = RunId::new("shell-run-2");
    second.item_id = ItemId::new("shell-item-2");

    let error = store
        .accept_shell_exec(&second)
        .expect_err("active session must reject direct shell");
    assert_eq!(error.code, haider_protocol::error::ErrorCode::Busy);
    assert!(error.retryable);
}
