#![allow(clippy::expect_used)]

use haider_protocol::agent::{AgentManifest, AgentRole, Grant, Placement};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::NodeKind;
use haider_protocol::ids::{
    AgentId, BranchId, DeviceId, EventId, ItemId, LeaseId, RunId, SessionId,
};
use haider_protocol::item::{CommandExecutionOrigin, ItemEvent, ToolStatus, TurnItem};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::{DeliveryMode, EventPayload};
use haider_store::{
    BranchCreateCommand, DelegationRecord, DelegationState, EventStore, SessionCreateCommand,
    ShellExecAcceptCommand, ShellExecAcceptOutcome, Store, TurnAcceptCommand, TurnAcceptOutcome,
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
            cache_policy: Default::default(),
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
        branch_id: None,
        agent_id: None,
        run_id: RunId::new("shell-run-1"),
        item_id: ItemId::new("shell-item-1"),
        command: body.into(),
        running_event_id: EventId::new("shell-running-1"),
        item_event_id: EventId::new("shell-item-started-1"),
        active_event_id: EventId::new("shell-active-1"),
        device_id: DeviceId::new("test-daemon"),
    }
}

fn delegation(parent: &SessionId, child: &SessionId, agent: AgentId) -> DelegationRecord {
    DelegationRecord {
        agent_id: agent.clone(),
        child_session_id: child.clone(),
        child_run_id: RunId::new("child-run"),
        parent_session_id: parent.clone(),
        parent_run_id: RunId::new("parent-run"),
        parent_branch_id: None,
        call_id: "child-call".into(),
        tool_item_id: ItemId::new("child-tool-item"),
        parent_agent_id: None,
        root_session_id: parent.clone(),
        depth: 1,
        task: "test child".into(),
        prompt: "test child prompt".into(),
        manifest: AgentManifest {
            agent,
            role: AgentRole::Subagent,
            task: "test child".into(),
            callsign: Some("SHELL-CHILD".into()),
            model_profile: "fake-v1".into(),
            grant: Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: Some(4096),
            placement: Placement::Local,
            lease: LeaseId::new("child-lease"),
            fencing_epoch: 1,
            attempt: 0,
            parent: None,
            coordinates: None,
        },
        state: DelegationState::Spawned,
        report: None,
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
        [2, 3, 4, 5]
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
    let origin = match &payloads[2] {
        EventPayload::Item(ItemEvent::Completed { item, .. }) => {
            haider_protocol::item::UserCommandOriginV1::from_extension_item(item)
                .expect("typed user-command origin")
        }
        other => panic!("expected user-command origin marker, got {other:?}"),
    };
    assert_eq!(origin.command_item_id, ItemId::new("shell-item-1"));
    assert_eq!(origin.call_id, "shell-command-1");
    assert_eq!(origin.origin, CommandExecutionOrigin::UserCommand);
    assert!(matches!(
        &payloads[2],
        EventPayload::Item(ItemEvent::Completed { item_id, .. })
            if item_id == &ItemId::new("user-command-origin-shell-item-1")
    ));
    assert!(!envelopes[2].render.ui);
    assert!(envelopes[2].render.durable);
    assert_eq!(envelopes[2].render.prompt, PromptRender::Omit);
    assert!(envelopes.iter().all(|envelope| {
        envelope.run_id == Some(RunId::new("shell-run-1"))
            && envelope.branch_id.is_none()
            && envelope.agent_id.is_none()
    }));
    assert!(matches!(
        payloads[3],
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
    assert_eq!(store.latest_seq(&session_id).expect("head"), 5);
}

/// MUTATION CHECK: trust client-supplied prompt scope without checking the
/// branch table or the child-session delegation relation. Expected runtime
/// failure: an unreachable scope commits, or a real child agent is rejected.
#[test]
fn shell_acceptance_rejects_unreachable_scope_and_accepts_exact_child_agent() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let root_session = SessionId::new("shell-scope-root");
    create(&store, &root_session);

    let mut unknown_branch = command(&store, &root_session, "printf branch");
    unknown_branch.branch_id = Some(BranchId::new("missing-branch"));
    let error = store
        .accept_shell_exec(&unknown_branch)
        .expect_err("unknown branch must not execute");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );

    let mut forged_root_agent = command(&store, &root_session, "printf agent");
    forged_root_agent.agent_id = Some(AgentId::new("forged-agent"));
    let error = store
        .accept_shell_exec(&forged_root_agent)
        .expect_err("root session must reject an agent scope");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
    assert_eq!(store.latest_seq(&root_session).expect("root head"), 1);

    let child_session = SessionId::new("shell-scope-child");
    create(&store, &child_session);
    let child_agent = AgentId::new("shell-child-agent");
    store
        .create_delegation(&delegation(
            &root_session,
            &child_session,
            child_agent.clone(),
        ))
        .expect("create child delegation");

    let mut child_command = command(&store, &child_session, "printf child");
    child_command.agent_id = Some(child_agent.clone());
    let ShellExecAcceptOutcome::Committed { envelopes, .. } = store
        .accept_shell_exec(&child_command)
        .expect("exact child scope commits")
    else {
        panic!("fresh child command must commit");
    };
    assert!(
        envelopes
            .iter()
            .all(|envelope| envelope.agent_id.as_ref() == Some(&child_agent))
    );

    let mut omitted_child_agent = command(&store, &child_session, "printf omitted");
    omitted_child_agent.command_id = "shell-command-omitted-agent".into();
    omitted_child_agent.request_digest = "digest-omitted-agent".into();
    omitted_child_agent.request_json = r#"{"command":"printf omitted"}"#.into();
    let error = store
        .accept_shell_exec(&omitted_child_agent)
        .expect_err("child session must require its durable agent scope");
    assert_eq!(
        error.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
}

/// A registered named branch is a legal prompt scope and remains stamped on
/// every atomically accepted user-command prefix envelope.
#[test]
fn shell_acceptance_preserves_a_registered_named_branch() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("shell-registered-branch");
    create(&store, &session_id);
    let generation = store.worker_generation();
    let main_run = RunId::new("shell-branch-main-run");
    let turn = store
        .accept_turn(&TurnAcceptCommand {
            command_id: "shell-branch-main-turn".into(),
            request_digest: "shell-branch-main-digest".into(),
            request_json: r#"{"text":"branch base"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: main_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "branch base".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("shell-branch-main-queued"),
            user_event_id: EventId::new("shell-branch-main-user"),
            active_event_id: EventId::new("shell-branch-main-active"),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("accept branch base turn");
    let TurnAcceptOutcome::Committed { envelopes, .. } = turn else {
        panic!("fresh base turn commits");
    };
    let (fork_node_id, fork_seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            matches!(node.kind, NodeKind::UserTurn { .. }).then_some((node.node, envelope.seq))
        })
        .expect("base user node");
    let mut done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("shell-branch-main-done"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(main_run),
        agent_id: None,
        device_id: DeviceId::new("test-daemon"),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("done payload"),
    }];
    store
        .append_worker(&mut done)
        .expect("terminalize base turn");

    let branch_id = BranchId::new("registered-shell-branch");
    store
        .create_branch(&BranchCreateCommand {
            command_id: "create-registered-shell-branch".into(),
            request_digest: "create-registered-shell-branch-digest".into(),
            request_json: r#"{"branch":"registered-shell-branch"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id,
            fork_seq,
            name: Some("Registered shell branch".into()),
            event_id: EventId::new("registered-shell-branch-created"),
            device_id: DeviceId::new("test-daemon"),
        })
        .expect("create registered branch");

    let mut scoped = command(&store, &session_id, "printf branch");
    scoped.branch_id = Some(branch_id.clone());
    let ShellExecAcceptOutcome::Committed { envelopes, .. } = store
        .accept_shell_exec(&scoped)
        .expect("registered branch accepts shell command")
    else {
        panic!("fresh scoped command commits");
    };
    assert!(
        envelopes
            .iter()
            .all(|envelope| envelope.branch_id.as_ref() == Some(&branch_id))
    );
    let head = store.latest_seq(&session_id).expect("scoped head");
    assert!(matches!(
        store
            .accept_shell_exec(&scoped)
            .expect("scoped receipt replay"),
        ShellExecAcceptOutcome::IdempotentReplay { .. }
    ));
    assert_eq!(store.latest_seq(&session_id).expect("replayed head"), head);
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
