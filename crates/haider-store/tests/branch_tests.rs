#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::branch::BranchCreated;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{BranchId, DeviceId, EventId, NodeId, RunId, SessionId};
use haider_protocol::state::RunState;
use haider_store::{
    BranchCreateCommand, BranchCreateOutcome, EventStore, SessionCreateCommand, Store,
    TurnAcceptCommand, TurnAcceptOutcome,
};

fn create_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-digest-{session_id}"),
            request_json: format!(r#"{{"session_id":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            system_prompt_version: "branch-test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new("branch-test-device"),
        })
        .expect("create session");
}

fn turn_command(
    store: &Store,
    session_id: &SessionId,
    command_id: &str,
    run_id: &str,
    branch_id: Option<BranchId>,
) -> TurnAcceptCommand {
    let request_json = serde_json::json!({
        "session_id": session_id,
        "worker_generation": store.worker_generation(),
        "branch_id": branch_id,
        "text": command_id,
        "attachments": [],
        "mode": "queue",
    })
    .to_string();
    TurnAcceptCommand {
        command_id: command_id.into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: RunId::new(run_id),
        agent_id: None,
        branch_id,
        text: command_id.into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new(format!("queued-{command_id}")),
        user_event_id: EventId::new(format!("user-{command_id}")),
        active_event_id: EventId::new(format!("active-{command_id}")),
        device_id: DeviceId::new("branch-test-device"),
    }
}

fn terminal_main_turn(store: &Store, session_id: &SessionId, suffix: &str) -> (RunId, NodeId, u64) {
    let run_id = RunId::new(format!("run-{suffix}"));
    let outcome = store
        .accept_turn(&turn_command(
            store,
            session_id,
            &format!("turn-{suffix}"),
            run_id.as_str(),
            None,
        ))
        .expect("accept main turn");
    let TurnAcceptOutcome::Committed { envelopes, .. } = outcome else {
        panic!("fresh turn commits");
    };
    let (node_id, node_seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq))
        })
        .expect("accepted user node");
    let mut done = [raw(
        store,
        session_id,
        None,
        Some(run_id.clone()),
        format!("done-{suffix}"),
        EventPayload::RunState(RunState::Done),
    )];
    store
        .append_worker(&mut done)
        .expect("terminalize main turn");
    (run_id, node_id, node_seq)
}

#[allow(clippy::too_many_arguments)]
fn branch_command(
    store: &Store,
    session_id: &SessionId,
    command_id: &str,
    branch_id: &str,
    source_branch_id: Option<BranchId>,
    fork_node_id: NodeId,
    fork_seq: u64,
    name: Option<&str>,
) -> BranchCreateCommand {
    let request_json = serde_json::json!({
        "session_id": session_id,
        "worker_generation": store.worker_generation(),
        "source_branch_id": source_branch_id,
        "fork_node_id": fork_node_id,
        "fork_seq": fork_seq,
        "name": name,
    })
    .to_string();
    BranchCreateCommand {
        command_id: command_id.into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        branch_id: BranchId::new(branch_id),
        source_branch_id,
        fork_node_id,
        fork_seq,
        name: name.map(str::to_owned),
        event_id: EventId::new(format!("event-{command_id}")),
        device_id: DeviceId::new("branch-test-device"),
    }
}

fn raw(
    store: &Store,
    session_id: &SessionId,
    branch_id: Option<BranchId>,
    run_id: Option<RunId>,
    event_id: String,
    payload: EventPayload,
) -> haider_protocol::envelope::RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id,
        run_id,
        agent_id: None,
        device_id: DeviceId::new("branch-test-device"),
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
        payload: serde_json::to_value(payload).expect("payload"),
    }
}

/// MUTATION CHECK: move branch row, event, or receipt into a separate
/// transaction. Expected RUNTIME failure: the collision case leaves one of
/// those three durable surfaces behind, or restart replay changes coordinates.
#[test]
fn branch_create_receipt_event_and_registry_are_one_atomic_fact() {
    let root = tempfile::tempdir().expect("profile");
    let session_id = SessionId::new("branch-r2-session");
    let store = Store::open(root.path()).expect("store");
    create_session(&store, &session_id);
    let (_, fork_node, fork_seq) = terminal_main_turn(&store, &session_id, "fork");
    let command = branch_command(
        &store,
        &session_id,
        "create-branch-a",
        "branch-a",
        None,
        fork_node.clone(),
        fork_seq,
        Some("Plan A"),
    );
    let BranchCreateOutcome::Committed { created, envelope } =
        store.create_branch(&command).expect("create branch")
    else {
        panic!("fresh branch commits");
    };
    assert_eq!(created.branch_id, BranchId::new("branch-a"));
    assert_eq!(created.created_seq, envelope.seq);
    let fact = BranchCreated::from_payload_value(&envelope.payload).expect("branch fact");
    assert_eq!(fact.branch.branch_id, created.branch_id);
    assert_eq!(
        store
            .branch(&session_id, &created.branch_id)
            .expect("branch"),
        Some(fact.branch)
    );
    let replay = store
        .branch_create_receipt(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )
        .expect("receipt")
        .expect("committed response");
    assert_eq!(replay, created);
    let mut same_generation_retry = command.clone();
    same_generation_retry.branch_id = BranchId::new("discarded-retry-id");
    same_generation_retry.event_id = EventId::new("discarded-retry-event");
    let BranchCreateOutcome::IdempotentReplay {
        created: same_generation,
    } = store
        .create_branch(&same_generation_retry)
        .expect("same-generation replay")
    else {
        panic!("retry is response-only");
    };
    assert_eq!(same_generation, created);

    drop(store);
    let reopened = Store::open(root.path()).expect("reopen");
    let restarted = reopened
        .branch_create_receipt(
            &command.command_id,
            &command.request_digest,
            &command.request_json,
        )
        .expect("restart receipt lookup")
        .expect("restart replay");
    assert_eq!(restarted, created);

    for changed_json in [
        r#"{"fork_node_id":"changed"}"#,
        r#"{"fork_seq":999}"#,
        r#"{"source_branch_id":"changed"}"#,
        r#"{"name":"changed"}"#,
    ] {
        let changed_digest = blake3::hash(changed_json.as_bytes()).to_hex();
        let error = reopened
            .branch_create_receipt(&command.command_id, changed_digest.as_ref(), changed_json)
            .expect_err("changed semantic body is rejected");
        assert_eq!(
            error.code,
            haider_protocol::error::ErrorCode::InvalidArgument
        );
    }

    let failed = branch_command(
        &reopened,
        &session_id,
        "create-branch-rollback",
        "branch-rollback",
        None,
        fork_node,
        fork_seq,
        None,
    );
    let mut failed = failed;
    failed.event_id = EventId::new(format!("created-{session_id}"));
    assert!(reopened.create_branch(&failed).is_err());
    assert!(
        reopened
            .branch(&session_id, &failed.branch_id)
            .expect("branch lookup")
            .is_none()
    );
    assert!(
        reopened
            .branch_create_receipt(
                &failed.command_id,
                &failed.request_digest,
                &failed.request_json,
            )
            .expect("receipt lookup")
            .is_none()
    );
}

/// MUTATION CHECK: validate a node id and sequence independently, or scan all
/// branch scopes, or omit the stable-boundary gate. Expected RUNTIME failure:
/// a cross-session/fabricated/sibling/child-agent/nonterminal coordinate below
/// creates a named ref.
#[test]
fn fork_coordinates_are_exact_lineage_root_agent_and_stable() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("branch-validation-session");
    create_session(&store, &session_id);
    let (_, first_node, first_seq) = terminal_main_turn(&store, &session_id, "first");
    let first = branch_command(
        &store,
        &session_id,
        "branch-first",
        "branch-first",
        None,
        first_node.clone(),
        first_seq,
        None,
    );
    store.create_branch(&first).expect("first branch");
    let (_, later_node, later_seq) = terminal_main_turn(&store, &session_id, "later");

    let same_fork = branch_command(
        &store,
        &session_id,
        "branch-same-fork",
        "branch-same-fork",
        None,
        first_node.clone(),
        first_seq,
        None,
    );
    store
        .create_branch(&same_fork)
        .expect("distinct commands may share a fork");

    let foreign_session = SessionId::new("branch-validation-foreign-session");
    create_session(&store, &foreign_session);
    let (_, foreign_node, foreign_seq) = terminal_main_turn(&store, &foreign_session, "foreign");
    let cross_session = branch_command(
        &store,
        &session_id,
        "branch-cross-session",
        "branch-cross-session",
        None,
        foreign_node,
        foreign_seq,
        None,
    );
    assert!(store.create_branch(&cross_session).is_err());

    let fabricated = branch_command(
        &store,
        &session_id,
        "branch-fabricated",
        "branch-fabricated",
        None,
        NodeId::new("fabricated"),
        first_seq,
        None,
    );
    assert!(store.create_branch(&fabricated).is_err());
    let mismatched = branch_command(
        &store,
        &session_id,
        "branch-mismatch",
        "branch-mismatch",
        None,
        first_node.clone(),
        later_seq,
        None,
    );
    assert!(store.create_branch(&mismatched).is_err());
    let unstable_run = RunId::new("unstable-run");
    let unstable_node = NodeId::new("unstable-node");
    let mut unstable = [raw(
        &store,
        &session_id,
        None,
        Some(unstable_run),
        "unstable-node-event".into(),
        EventPayload::NodeCommitted(TreeNode {
            node: unstable_node.clone(),
            parent: Some(later_node.clone()),
            kind: NodeKind::AssistantCommit {
                text: "partial".into(),
                verdict: haider_protocol::verify::VerifyVerdict::Unverified,
            },
        }),
    )];
    store.append(&mut unstable).expect("append unstable node");
    let nonterminal = branch_command(
        &store,
        &session_id,
        "branch-nonterminal",
        "branch-nonterminal",
        None,
        unstable_node,
        unstable[0].seq,
        None,
    );
    assert!(store.create_branch(&nonterminal).is_err());
    let sibling = branch_command(
        &store,
        &session_id,
        "branch-sibling",
        "branch-sibling",
        Some(BranchId::new("branch-first")),
        later_node,
        later_seq,
        None,
    );
    assert!(store.create_branch(&sibling).is_err());

    let child_run = RunId::new("child-run");
    let child_node = NodeId::new("child-node");
    let mut child = [raw(
        &store,
        &session_id,
        None,
        Some(child_run.clone()),
        "child-node-event".into(),
        EventPayload::NodeCommitted(TreeNode {
            node: child_node.clone(),
            parent: Some(first_node),
            kind: NodeKind::AssistantCommit {
                text: "child".into(),
                verdict: haider_protocol::verify::VerifyVerdict::Unverified,
            },
        }),
    )];
    child[0].agent_id = Some(haider_protocol::ids::AgentId::new("child-agent"));
    store.append(&mut child).expect("append child node");
    let child_fork = branch_command(
        &store,
        &session_id,
        "branch-child",
        "branch-child",
        None,
        child_node,
        child[0].seq,
        None,
    );
    assert!(store.create_branch(&child_fork).is_err());
}

/// MUTATION CHECK: choose the session-global latest node as a named-branch
/// parent, fail to stamp a run batch, or update the head after commit.
/// Expected RUNTIME failure: the assertions below expose cross-branch parent,
/// scope, or head coordinates.
#[test]
fn branch_turns_parent_stamp_and_advance_only_the_selected_head() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("branch-turn-session");
    create_session(&store, &session_id);
    let (_, fork_node, fork_seq) = terminal_main_turn(&store, &session_id, "base");
    let branch_id = BranchId::new("branch-turns");
    store
        .create_branch(&branch_command(
            &store,
            &session_id,
            "create-branch-turns",
            branch_id.as_str(),
            None,
            fork_node.clone(),
            fork_seq,
            None,
        ))
        .expect("create branch");

    let branch_run = RunId::new("branch-run");
    let TurnAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "branch-turn",
            branch_run.as_str(),
            Some(branch_id.clone()),
        ))
        .expect("accept branch turn")
    else {
        panic!("fresh turn commits");
    };
    assert_eq!(accepted.branch_id, Some(branch_id.clone()));
    for envelope in &envelopes {
        if matches!(
            serde_json::from_value::<EventPayload>(envelope.payload.clone()),
            Ok(EventPayload::SessionState(_))
        ) {
            assert_eq!(envelope.branch_id, None);
        } else {
            assert_eq!(envelope.branch_id, Some(branch_id.clone()));
        }
    }
    let (user_node, user_seq, parent) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq, node.parent))
        })
        .expect("branch user node");
    assert_eq!(parent, Some(fork_node));
    let descriptor = store
        .branch(&session_id, &branch_id)
        .expect("branch lookup")
        .expect("branch descriptor");
    assert_eq!(
        (descriptor.head_node_id, descriptor.head_seq),
        (user_node.clone(), user_seq)
    );

    let main = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "queued-main",
            "queued-main-run",
            None,
        ))
        .expect("accept main");
    let TurnAcceptOutcome::Committed {
        accepted,
        envelopes,
    } = main
    else {
        panic!("main turn commits");
    };
    assert_eq!(accepted.branch_id, None);
    assert!(
        envelopes
            .iter()
            .all(|envelope| envelope.branch_id.is_none())
    );

    // Non-degenerate half: main has now advanced PAST the fork, so the branch
    // head and the session-global main head name different nodes. A second
    // branch turn must parent on the branch head, never main's latest node.
    let mut branch_done = [raw(
        &store,
        &session_id,
        Some(branch_id.clone()),
        Some(branch_run.clone()),
        "branch-turn-done".into(),
        EventPayload::RunState(RunState::Done),
    )];
    store
        .append_worker(&mut branch_done)
        .expect("terminalize branch turn");
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "branch-turn-2",
            "branch-run-2",
            Some(branch_id.clone()),
        ))
        .expect("accept second branch turn")
    else {
        panic!("second branch turn commits");
    };
    let (second_node, second_seq, second_parent) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq, node.parent))
        })
        .expect("second branch user node");
    assert_eq!(second_parent, Some(user_node));
    let descriptor = store
        .branch(&session_id, &branch_id)
        .expect("branch lookup")
        .expect("branch descriptor");
    assert_eq!(
        (descriptor.head_node_id, descriptor.head_seq),
        (second_node, second_seq)
    );
}

/// MUTATION CHECK: admit a second nonterminal run onto the same named ref.
/// Expected RUNTIME failure: its user node advances the head before the first
/// run's later assistant node has an honest immutable parent order.
#[test]
fn a_named_branch_serializes_nonterminal_turns_but_other_branches_may_queue() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let session_id = SessionId::new("branch-queue-order-session");
    create_session(&store, &session_id);
    let (_, fork_node, fork_seq) = terminal_main_turn(&store, &session_id, "queue-base");
    let branch_a = BranchId::new("queue-branch-a");
    let branch_b = BranchId::new("queue-branch-b");
    for (command, branch) in [
        ("create-queue-a", branch_a.clone()),
        ("create-queue-b", branch_b.clone()),
    ] {
        store
            .create_branch(&branch_command(
                &store,
                &session_id,
                command,
                branch.as_str(),
                None,
                fork_node.clone(),
                fork_seq,
                None,
            ))
            .expect("create queue branch");
    }
    let first_run = RunId::new("queue-a-first");
    store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "queue-a-first",
            first_run.as_str(),
            Some(branch_a.clone()),
        ))
        .expect("accept first A run");
    let error = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "queue-a-second-too-early",
            "queue-a-second-too-early",
            Some(branch_a.clone()),
        ))
        .expect_err("same named branch serializes nonterminal runs");
    assert_eq!(error.code, haider_protocol::error::ErrorCode::Busy);
    assert!(error.retryable);

    let TurnAcceptOutcome::Committed { accepted, .. } = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "queue-b-while-a-active",
            "queue-b-while-a-active",
            Some(branch_b),
        ))
        .expect("a distinct named branch may queue")
    else {
        panic!("fresh B turn commits");
    };
    assert_eq!(
        accepted.disposition,
        haider_store::TurnAdmissionDisposition::Queued
    );

    let first_descriptor = store
        .branch(&session_id, &branch_a)
        .expect("branch lookup")
        .expect("branch A");
    let assistant_node = NodeId::new("queue-a-first-assistant");
    let mut finish_first = vec![
        raw(
            &store,
            &session_id,
            Some(branch_a.clone()),
            Some(first_run.clone()),
            "queue-a-first-assistant-event".into(),
            EventPayload::NodeCommitted(TreeNode {
                node: assistant_node.clone(),
                parent: Some(first_descriptor.head_node_id),
                kind: NodeKind::AssistantCommit {
                    text: "first complete".into(),
                    verdict: haider_protocol::verify::VerifyVerdict::Unverified,
                },
            }),
        ),
        raw(
            &store,
            &session_id,
            Some(branch_a.clone()),
            Some(first_run),
            "queue-a-first-done".into(),
            EventPayload::RunState(RunState::Done),
        ),
    ];
    store
        .append_worker(&mut finish_first)
        .expect("finish first A run");
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&turn_command(
            &store,
            &session_id,
            "queue-a-second-after-terminal",
            "queue-a-second-after-terminal",
            Some(branch_a),
        ))
        .expect("accept second A after terminal")
    else {
        panic!("second A turn commits after terminal");
    };
    let parent = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some(node.parent)
        })
        .expect("second A user node");
    assert_eq!(parent, Some(assistant_node));
}
