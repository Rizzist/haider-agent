#![allow(clippy::expect_used)]

use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{
    EventEnvelope, PromptRender, RawEnvelope, RenderTargets, SCHEMA_VERSION,
};
use haider_protocol::error::ErrorCode;
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{BranchId, DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::session_fork::{SessionForkProvenance, SessionForked};
use haider_protocol::state::{RunState, SessionState};
use haider_protocol::tool::{AttachmentBlock, BoundedResult, ToolResultStatus};
use haider_protocol::verify::VerifyVerdict;
use haider_store::{
    BranchCreateCommand, BranchCreateOutcome, Cas, SessionCreateCommand, SessionForkOutcome,
    SessionPromptForkCommand, Store, TurnAcceptCommand, TurnAcceptOutcome,
};

const DEVICE: &str = "session-prompt-fork-test-device";
const GOLDEN: &str = include_str!("fixtures/session_prompt_fork_golden.json");

#[derive(Clone, Copy)]
enum CompletedTurn {
    Successful,
    ToolHeavy,
    Errored,
    Cancelled,
    Interrupted,
}

struct AcceptedFixture {
    run_id: RunId,
    node_id: NodeId,
    node_seq: u64,
    user_seq: u64,
}

fn create_session(store: &Store, session_id: &SessionId) {
    store
        .create_session(&SessionCreateCommand {
            command_id: format!("create-{session_id}"),
            request_digest: format!("create-digest-{session_id}"),
            request_json: format!(r#"{{"session_id":"{session_id}"}}"#),
            session_id: session_id.clone(),
            cwd: "/tmp/prompt-fork".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "session-prompt-fork-test-v1".into(),
            event_id: EventId::new(format!("created-{session_id}")),
            device_id: DeviceId::new(DEVICE),
        })
        .expect("create source session");
}

fn accept_turn(
    store: &Store,
    session_id: &SessionId,
    label: &str,
    text: &str,
    attachments: Vec<AttachmentBlock>,
    branch_id: Option<BranchId>,
    mode: DeliveryMode,
    run_id: Option<RunId>,
) -> AcceptedFixture {
    let run_id = run_id.unwrap_or_else(|| RunId::new(format!("run-{session_id}-{label}")));
    let request_json = serde_json::json!({
        "session_id": session_id,
        "worker_generation": store.worker_generation(),
        "branch_id": branch_id,
        "text": text,
        "attachments": attachments,
        "mode": mode,
    })
    .to_string();
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(&TurnAcceptCommand {
            command_id: format!("turn-{session_id}-{label}"),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: run_id.clone(),
            agent_id: None,
            branch_id,
            text: text.into(),
            attachments,
            mode,
            queued_event_id: EventId::new(format!("queued-{session_id}-{label}")),
            user_event_id: EventId::new(format!("user-{session_id}-{label}")),
            active_event_id: EventId::new(format!("active-{session_id}-{label}")),
            device_id: DeviceId::new(DEVICE),
        })
        .expect("accept turn")
    else {
        panic!("fresh turn must commit");
    };
    let user_seq = envelopes
        .iter()
        .find(|envelope| {
            matches!(
                serde_json::from_value::<EventPayload>(envelope.payload.clone()),
                Ok(EventPayload::UserMessage { .. })
            )
        })
        .expect("accepted user message")
        .seq;
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
    AcceptedFixture {
        run_id,
        node_id,
        node_seq,
        user_seq,
    }
}

fn raw(
    store: &Store,
    session_id: &SessionId,
    run_id: Option<RunId>,
    branch_id: Option<BranchId>,
    event_id: impl Into<String>,
    payload: EventPayload,
    prompt: PromptRender,
) -> RawEnvelope {
    EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id.into()),
        seq: 0,
        session_id: session_id.clone(),
        branch_id,
        run_id,
        agent_id: None,
        device_id: DeviceId::new(DEVICE),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt,
        },
        payload: serde_json::to_value(payload).expect("encode fixture payload"),
    }
}

fn completed_agent_facts(
    store: &Store,
    session_id: &SessionId,
    accepted: &AcceptedFixture,
    case: CompletedTurn,
) -> Vec<RawEnvelope> {
    let mut facts = Vec::new();
    let run = || Some(accepted.run_id.clone());
    let omitted = PromptRender::Omit;
    match case {
        CompletedTurn::Successful => {
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("assistant-item-{session_id}"),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(format!("assistant-item-{session_id}")),
                    item: TurnItem::AgentMessage {
                        text: "answer A".into(),
                    },
                }),
                PromptRender::Verbatim,
            ));
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("assistant-node-{session_id}"),
                EventPayload::NodeCommitted(TreeNode {
                    node: NodeId::new(format!("assistant-node-{session_id}")),
                    parent: Some(accepted.node_id.clone()),
                    kind: NodeKind::AssistantCommit {
                        text: "answer A".into(),
                        verdict: VerifyVerdict::NotApplicable,
                    },
                }),
                omitted,
            ));
        }
        CompletedTurn::ToolHeavy => {
            let mut parent = accepted.node_id.clone();
            for index in 1..=2 {
                let call_id = format!("tool-call-{session_id}-{index}");
                facts.push(raw(
                    store,
                    session_id,
                    run(),
                    None,
                    format!("tool-result-{session_id}-{index}"),
                    EventPayload::ToolResult {
                        call_id: call_id.clone(),
                        result: BoundedResult {
                            preview: format!("result {index}"),
                            truncated: false,
                            data: None,
                            artifact: None,
                            images: Vec::new(),
                            cursor: None,
                            status: ToolResultStatus::Completed,
                            reason: None,
                            presentation: None,
                        },
                    },
                    PromptRender::Verbatim,
                ));
                facts.push(raw(
                    store,
                    session_id,
                    run(),
                    None,
                    format!("tool-item-{session_id}-{index}"),
                    EventPayload::Item(ItemEvent::Completed {
                        item_id: ItemId::new(format!("tool-item-{session_id}-{index}")),
                        item: TurnItem::ToolCall {
                            call_id,
                            name: format!("tool_{index}"),
                            args: serde_json::json!({"index": index}),
                            status: ToolStatus::Completed,
                        },
                    }),
                    PromptRender::Verbatim,
                ));
                let node_id = NodeId::new(format!("tool-node-{session_id}-{index}"));
                facts.push(raw(
                    store,
                    session_id,
                    run(),
                    None,
                    format!("tool-node-event-{session_id}-{index}"),
                    EventPayload::NodeCommitted(TreeNode {
                        node: node_id.clone(),
                        parent: Some(parent),
                        kind: NodeKind::ToolExchange {
                            tool: format!("tool_{index}"),
                            summary: format!("tool {index} completed"),
                            artifact: None,
                        },
                    }),
                    omitted,
                ));
                parent = node_id;
            }
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("assistant-item-{session_id}"),
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(format!("assistant-item-{session_id}")),
                    item: TurnItem::AgentMessage {
                        text: "tool-backed answer A".into(),
                    },
                }),
                PromptRender::Verbatim,
            ));
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("assistant-node-{session_id}"),
                EventPayload::NodeCommitted(TreeNode {
                    node: NodeId::new(format!("assistant-node-{session_id}")),
                    parent: Some(parent),
                    kind: NodeKind::AssistantCommit {
                        text: "tool-backed answer A".into(),
                        verdict: VerifyVerdict::NotApplicable,
                    },
                }),
                omitted,
            ));
        }
        CompletedTurn::Errored => {
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("run-failed-{session_id}"),
                EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "provider rejected A".into(),
                    retryable: false,
                    presentation: None,
                },
                omitted,
            ));
        }
        CompletedTurn::Cancelled | CompletedTurn::Interrupted => {
            facts.push(raw(
                store,
                session_id,
                run(),
                None,
                format!("cancelling-{session_id}"),
                EventPayload::RunState(RunState::Cancelling),
                omitted,
            ));
        }
    }
    let terminal = match case {
        CompletedTurn::Successful | CompletedTurn::ToolHeavy => RunState::Done,
        CompletedTurn::Errored => RunState::Errored,
        CompletedTurn::Cancelled | CompletedTurn::Interrupted => RunState::Cancelled,
    };
    facts.push(raw(
        store,
        session_id,
        run(),
        None,
        format!("terminal-{session_id}"),
        EventPayload::RunState(terminal),
        omitted,
    ));
    facts.push(raw(
        store,
        session_id,
        None,
        None,
        format!("idle-{session_id}"),
        EventPayload::SessionState(SessionState::Idle {
            interrupted: matches!(case, CompletedTurn::Interrupted),
        }),
        omitted,
    ));
    facts
}

fn append_completed_turn(
    store: &Store,
    session_id: &SessionId,
    accepted: &AcceptedFixture,
    case: CompletedTurn,
) {
    let mut facts = completed_agent_facts(store, session_id, accepted, case);
    store
        .append_worker(&mut facts)
        .expect("append completed turn fixture");
}

fn prompt_command(
    store: &Store,
    source: &SessionId,
    branch_id: Option<BranchId>,
    prompt_seq: u64,
    command_id: &str,
    child_id: &str,
) -> SessionPromptForkCommand {
    let request_json = serde_json::json!({
        "session_id": source,
        "worker_generation": store.worker_generation(),
        "source_branch_id": branch_id,
        "prompt": {"seq": prompt_seq},
        "name": "editable fork",
    })
    .to_string();
    SessionPromptForkCommand {
        command_id: command_id.into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        source_session_id: source.clone(),
        session_id: SessionId::new(child_id),
        worker_generation: store.worker_generation(),
        source_branch_id: branch_id,
        prompt_seq,
        name: Some("editable fork".into()),
        audit_event_id: EventId::new(format!("audit-{child_id}")),
        device_id: DeviceId::new(DEVICE),
    }
}

fn event_label(envelope: &RawEnvelope) -> String {
    let payload = &envelope.payload;
    let kind = payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        .expect("fixture payload type");
    match kind {
        "run_state" => format!(
            "run_state:{}",
            payload["state"].as_str().expect("run state")
        ),
        "session_state" if payload["state"] == "idle" => format!(
            "session_state:idle:{}",
            payload["interrupted"].as_bool().expect("idle interrupted")
        ),
        "session_state" => format!(
            "session_state:{}",
            payload["state"].as_str().expect("session state")
        ),
        "item" => format!(
            "item:{}:{}",
            payload["event"].as_str().expect("item event"),
            payload["item"]["item"].as_str().expect("item kind")
        ),
        "node_committed" => format!(
            "node_committed:{}",
            payload["kind"]["kind"].as_str().expect("node kind")
        ),
        other => other.into(),
    }
}

fn golden_case(name: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(GOLDEN).expect("golden fixture")[name].clone()
}

#[test]
fn golden_prompt_cuts_cover_terminal_boundaries_and_first_prompt() {
    for (name, case) in [
        ("successful", Some(CompletedTurn::Successful)),
        ("tool_heavy", Some(CompletedTurn::ToolHeavy)),
        ("errored", Some(CompletedTurn::Errored)),
        ("cancelled", Some(CompletedTurn::Cancelled)),
        ("interrupted", Some(CompletedTurn::Interrupted)),
        ("first_prompt", None),
    ] {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let source = SessionId::new(format!("golden-{name}-source"));
        create_session(&store, &source);
        if let Some(case) = case {
            let a = accept_turn(
                &store,
                &source,
                "a",
                "prompt A",
                Vec::new(),
                None,
                DeliveryMode::Queue,
                None,
            );
            append_completed_turn(&store, &source, &a, case);
        }
        let b = accept_turn(
            &store,
            &source,
            "b",
            "editable prompt B",
            Vec::new(),
            None,
            DeliveryMode::Queue,
            None,
        );
        let command = prompt_command(
            &store,
            &source,
            None,
            b.user_seq,
            &format!("fork-{name}"),
            &format!("golden-{name}-child"),
        );
        let SessionForkOutcome::Committed { created, envelopes } = store
            .fork_session_from_prompt(&command)
            .expect("prompt fork")
        else {
            panic!("fresh prompt fork must commit");
        };
        let copied_len = usize::try_from(created.fork_seq).expect("fork seq fits usize");
        let copied = envelopes[..copied_len]
            .iter()
            .map(event_label)
            .collect::<Vec<_>>();
        let expected = golden_case(name)["copied"]
            .as_array()
            .expect("golden copied events")
            .iter()
            .map(|value| value.as_str().expect("golden event label").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(copied, expected, "golden cut for {name}");
        assert!(
            copied.iter().all(|label| label != "user_message") || name != "first_prompt",
            "the first-prompt child has no user transcript"
        );
        assert_eq!(
            created.forked_from,
            Some(SessionForkProvenance {
                session_id: source.clone(),
                seq: b.user_seq,
            })
        );
        assert_eq!(
            created.draft.as_ref().map(|draft| draft.text.as_str()),
            Some("editable prompt B")
        );
        let audit = envelopes
            .iter()
            .filter_map(|envelope| SessionForked::from_payload_value(&envelope.payload))
            .next_back()
            .expect("durable prompt-fork audit");
        assert_eq!(audit.forked_from, created.forked_from);
        assert!(
            envelopes[..copied_len]
                .iter()
                .all(|envelope| envelope.seq < b.user_seq),
            "B and its admission batch remain outside the child prefix"
        );
    }
}

#[test]
fn queued_and_steered_prompts_are_retryably_unstable() {
    for (label, mode, same_run) in [
        ("queued", DeliveryMode::Queue, false),
        ("steered", DeliveryMode::Steer, true),
    ] {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let source = SessionId::new(format!("unstable-{label}-source"));
        create_session(&store, &source);
        let a = accept_turn(
            &store,
            &source,
            "a",
            "active A",
            Vec::new(),
            None,
            DeliveryMode::Queue,
            None,
        );
        let b = accept_turn(
            &store,
            &source,
            "b",
            "unstable B",
            Vec::new(),
            None,
            mode,
            same_run.then_some(a.run_id),
        );
        let error = store
            .fork_session_from_prompt(&prompt_command(
                &store,
                &source,
                None,
                b.user_seq,
                &format!("unstable-{label}-command"),
                &format!("unstable-{label}-child"),
            ))
            .expect_err("queued/steered cut must be refused");
        assert_eq!(error.code, ErrorCode::ForkCutUnstable);
        assert!(error.retryable);
        assert_eq!(golden_case("queued_prompt")["outcome"], error.code.as_str());
    }
}

#[test]
fn prompt_on_another_branch_is_a_typed_structural_cut_error() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("wrong-branch-source");
    create_session(&store, &source);
    let a = accept_turn(
        &store,
        &source,
        "a",
        "branch root",
        Vec::new(),
        None,
        DeliveryMode::Queue,
        None,
    );
    append_completed_turn(&store, &source, &a, CompletedTurn::Successful);
    let branch_id = BranchId::new("branch-b");
    let request_json = serde_json::json!({
        "session_id": &source,
        "branch_id": &branch_id,
        "fork_node_id": &a.node_id,
        "fork_seq": a.node_seq,
    })
    .to_string();
    let BranchCreateOutcome::Committed { .. } = store
        .create_branch(&BranchCreateCommand {
            command_id: "create-branch-b".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: source.clone(),
            worker_generation: store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: a.node_id,
            fork_seq: a.node_seq,
            name: Some("branch B".into()),
            event_id: EventId::new("branch-b-created"),
            device_id: DeviceId::new(DEVICE),
        })
        .expect("create branch")
    else {
        panic!("fresh branch must commit");
    };
    let b = accept_turn(
        &store,
        &source,
        "b",
        "branch-only prompt B",
        Vec::new(),
        Some(branch_id),
        DeliveryMode::Queue,
        None,
    );
    let error = store
        .fork_session_from_prompt(&prompt_command(
            &store,
            &source,
            None,
            b.user_seq,
            "wrong-branch-command",
            "wrong-branch-child",
        ))
        .expect_err("wrong branch cut must fail");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(!error.retryable);
    assert_eq!(
        error.details.as_ref().expect("typed details")["kind"],
        "session_fork_invalid_cut"
    );
    assert_eq!(
        error.details.as_ref().expect("typed details")["reason"],
        golden_case("another_branch")["reason"]
    );
}

fn cas_object_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root.join("cas"))
        .expect("CAS root")
        .map(|entry| entry.expect("CAS shard"))
        .filter(|entry| entry.path().is_dir())
        .map(|shard| {
            std::fs::read_dir(shard.path())
                .expect("CAS shard entries")
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_file())
                .count()
        })
        .sum()
}

#[test]
fn retry_reuses_child_cas_refs_stay_deduplicated_and_running_parent_cannot_change_child() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("durability-source");
    create_session(&store, &source);
    let artifact = store
        .put(b"one immutable shared attachment")
        .expect("CAS put");
    let attachment = AttachmentBlock::PastedText {
        artifact: artifact.clone(),
        lines: 1,
    };
    let a = accept_turn(
        &store,
        &source,
        "a",
        "A with shared bytes",
        vec![attachment.clone()],
        None,
        DeliveryMode::Queue,
        None,
    );
    append_completed_turn(&store, &source, &a, CompletedTurn::Successful);
    let b = accept_turn(
        &store,
        &source,
        "b",
        "B remains editable while its parent run continues",
        vec![attachment.clone()],
        None,
        DeliveryMode::Queue,
        None,
    );
    let before_objects = cas_object_count(root.path());
    let parent_at_cut = store
        .journal_replay(&source)
        .expect("parent snapshot at prompt cut");
    let command = prompt_command(
        &store,
        &source,
        None,
        b.user_seq,
        "durability-command",
        "durability-child",
    );
    let SessionForkOutcome::Committed { created, .. } = store
        .fork_session_from_prompt(&command)
        .expect("fork while B is active")
    else {
        panic!("fresh prompt fork must commit");
    };
    assert_eq!(
        store.journal_replay(&source).expect("parent after fork"),
        parent_at_cut,
        "the parent records no reverse fork edge and stays byte-identical"
    );
    assert_eq!(cas_object_count(root.path()), before_objects);
    assert_eq!(
        created.draft.as_ref().expect("editable draft").attachments,
        vec![attachment]
    );
    let child_before = store
        .journal_replay(&created.session_id)
        .expect("child before parent continues");
    let session_count = store.session_ids().expect("session ids").len();

    let mut replay = command.clone();
    replay.session_id = SessionId::new("must-not-be-created");
    replay.audit_event_id = EventId::new("must-not-be-written");
    let SessionForkOutcome::IdempotentReplay { created: replayed } = store
        .fork_session_from_prompt(&replay)
        .expect("same command replays")
    else {
        panic!("same command must be response-only");
    };
    assert_eq!(replayed.session_id, created.session_id);
    assert_eq!(
        store.session_ids().expect("session ids").len(),
        session_count
    );

    let mut conflict = command.clone();
    conflict.prompt_seq = a.user_seq;
    conflict.request_json.push(' ');
    conflict.request_digest = blake3::hash(conflict.request_json.as_bytes())
        .to_hex()
        .to_string();
    let conflict = store
        .fork_session_from_prompt(&conflict)
        .expect_err("same command with different input conflicts");
    assert_eq!(conflict.code, ErrorCode::InvalidArgument);
    assert!(!conflict.retryable);

    let mut later = completed_agent_facts(&store, &source, &b, CompletedTurn::Successful);
    for envelope in &mut later {
        envelope.event_id = EventId::new(format!("later-{}", envelope.event_id));
    }
    store
        .append_worker(&mut later)
        .expect("parent B continues and terminalizes");
    assert_eq!(
        store
            .journal_replay(&created.session_id)
            .expect("child after parent continues"),
        child_before
    );
    assert_eq!(cas_object_count(root.path()), before_objects);
    assert!(store.verify(&artifact));
    drop(store);

    let reopened = Store::open(root.path()).expect("reopen store with a new generation");
    assert_eq!(
        reopened
            .session_fork_provenance(&created.session_id)
            .expect("roster provenance projection"),
        created.forked_from
    );
    let SessionForkOutcome::IdempotentReplay {
        created: restarted_replay,
    } = reopened
        .fork_session_from_prompt(&command)
        .expect("receipt replay precedes the generation fence")
    else {
        panic!("cross-generation retry must be response-only");
    };
    assert_eq!(restarted_replay, created);
}

fn incomplete_tool_fact(
    store: &Store,
    source: &SessionId,
    run_id: &RunId,
    call_without_result: bool,
) -> RawEnvelope {
    if call_without_result {
        raw(
            store,
            source,
            Some(run_id.clone()),
            None,
            format!("unpaired-call-{source}"),
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("unpaired-call-{source}")),
                item: TurnItem::ToolCall {
                    call_id: "unpaired".into(),
                    name: "fs_read".into(),
                    args: serde_json::json!({}),
                    status: ToolStatus::Cancelled,
                },
            }),
            PromptRender::Verbatim,
        )
    } else {
        raw(
            store,
            source,
            Some(run_id.clone()),
            None,
            format!("orphan-result-{source}"),
            EventPayload::ToolResult {
                call_id: "orphan".into(),
                result: BoundedResult {
                    preview: "orphan".into(),
                    truncated: false,
                    data: None,
                    artifact: None,
                    images: Vec::new(),
                    cursor: None,
                    status: ToolResultStatus::Cancelled,
                    reason: Some("cancelled".into()),
                    presentation: None,
                },
            },
            PromptRender::Verbatim,
        )
    }
}

#[test]
fn terminal_boundaries_never_copy_an_unpaired_tool_half() {
    for (label, terminal, interrupted, call_without_result) in [
        ("cancelled-call", RunState::Cancelled, false, true),
        ("interrupted-call", RunState::Cancelled, true, true),
        ("errored-result", RunState::Errored, false, false),
    ] {
        let root = tempfile::tempdir().expect("profile");
        let store = Store::open(root.path()).expect("store");
        let source = SessionId::new(format!("unpaired-{label}-source"));
        create_session(&store, &source);
        let a = accept_turn(
            &store,
            &source,
            "a",
            "A opens an incomplete tool exchange",
            Vec::new(),
            None,
            DeliveryMode::Queue,
            None,
        );
        let mut facts = vec![
            incomplete_tool_fact(&store, &source, &a.run_id, call_without_result),
            raw(
                &store,
                &source,
                Some(a.run_id.clone()),
                None,
                format!("terminal-{source}"),
                EventPayload::RunState(terminal),
                PromptRender::Omit,
            ),
            raw(
                &store,
                &source,
                None,
                None,
                format!("idle-{source}"),
                EventPayload::SessionState(SessionState::Idle { interrupted }),
                PromptRender::Omit,
            ),
        ];
        store
            .append_worker(&mut facts)
            .expect("append incomplete terminal turn");
        let b = accept_turn(
            &store,
            &source,
            "b",
            "prompt B",
            Vec::new(),
            None,
            DeliveryMode::Queue,
            None,
        );
        let error = store
            .fork_session_from_prompt(&prompt_command(
                &store,
                &source,
                None,
                b.user_seq,
                &format!("unpaired-{label}-command"),
                &format!("unpaired-{label}-child"),
            ))
            .expect_err("unpaired tool boundary must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!error.retryable);
        assert_eq!(
            error.details.as_ref().expect("typed invalid cut")["kind"],
            "session_fork_invalid_cut"
        );
        assert_eq!(
            store.session_ids().expect("sessions after refusal").len(),
            1,
            "late pair validation must roll back the child"
        );
    }
}

#[test]
fn non_user_prompt_and_stale_generation_keep_their_typed_errors() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("typed-error-source");
    create_session(&store, &source);
    let b = accept_turn(
        &store,
        &source,
        "b",
        "prompt B",
        Vec::new(),
        None,
        DeliveryMode::Queue,
        None,
    );
    let non_user = store
        .fork_session_from_prompt(&prompt_command(
            &store,
            &source,
            None,
            b.node_seq,
            "non-user-command",
            "non-user-child",
        ))
        .expect_err("user node is not a UserMessage");
    assert_eq!(non_user.code, ErrorCode::InvalidArgument);
    assert_eq!(
        non_user.details.as_ref().expect("invalid cut details")["reason"],
        "not_user_prompt"
    );

    let mut stale = prompt_command(
        &store,
        &source,
        None,
        b.user_seq,
        "stale-command",
        "stale-child",
    );
    stale.worker_generation = stale.worker_generation.saturating_add(1);
    let stale = store
        .fork_session_from_prompt(&stale)
        .expect_err("stale generation must fail");
    assert_eq!(stale.code, ErrorCode::SingleWriterViolation);
    assert!(!stale.retryable);
}

#[test]
fn missing_prompt_coordinate_is_not_found() {
    let root = tempfile::tempdir().expect("profile");
    let store = Store::open(root.path()).expect("store");
    let source = SessionId::new("missing-prompt-source");
    create_session(&store, &source);
    let error = store
        .fork_session_from_prompt(&prompt_command(
            &store,
            &source,
            None,
            999,
            "missing-prompt-command",
            "missing-prompt-child",
        ))
        .expect_err("missing prompt must fail");
    assert_eq!(error.code, ErrorCode::SessionNotFound);
    assert!(!error.retryable);
}
