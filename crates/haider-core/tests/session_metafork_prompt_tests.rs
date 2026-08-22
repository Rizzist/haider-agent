#![allow(clippy::expect_used)]

use haider_core::{
    PromptHistoryCompiler, SessionCreateCommand, SessionForkCommand, SessionForkOutcome,
    SessionMetaforkCommit, SqliteStoreHandle, StoreHandle, TurnAcceptCommand, TurnAcceptOutcome,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::session_fork::{
    SessionMetaforkProposal, SessionMetaforkRemoval, SessionMetaforkReviewManifest,
};
use haider_protocol::state::RunState;

/// MUTATION CHECK: leave the selected copied envelope `prompt = verbatim`.
/// Expected RUNTIME failure: the child provider prompt contains the unique
/// chocolate text even though the raw child journal still correctly retains it.
#[tokio::test]
async fn metafork_content_is_in_child_journal_but_not_child_prompt() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let source = SessionId::new("prompt-metafork-parent");
    store
        .create_session(SessionCreateCommand {
            command_id: "create-prompt-source".into(),
            request_digest: "create-prompt-source-digest".into(),
            request_json: r#"{"source":"prompt"}"#.into(),
            session_id: source.clone(),
            cwd: "/tmp".into(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: "prompt-metafork-v1".into(),
            event_id: EventId::new("prompt-source-created"),
            device_id: DeviceId::new("prompt-metafork-device"),
        })
        .await
        .expect("create source");
    let source_run = RunId::new("prompt-source-run");
    let request_json = r#"{"text":"secret chocolate ganache ratio 7:3"}"#.to_owned();
    let TurnAcceptOutcome::Committed { envelopes, .. } = store
        .accept_turn(TurnAcceptCommand {
            command_id: "prompt-source-turn".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: source.clone(),
            worker_generation: store.worker_generation(),
            run_id: source_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "secret chocolate ganache ratio 7:3".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("prompt-source-queued"),
            user_event_id: EventId::new("prompt-source-user"),
            active_event_id: EventId::new("prompt-source-active"),
            device_id: DeviceId::new("prompt-metafork-device"),
        })
        .await
        .expect("accept source turn")
    else {
        panic!("fresh source turn commits");
    };
    let user_seq = envelopes
        .iter()
        .find(|envelope| envelope.payload.to_string().contains("secret chocolate"))
        .expect("source user message")
        .seq;
    let (fork_node_id, fork_seq) = envelopes
        .iter()
        .find_map(|envelope| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(envelope.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, envelope.seq))
        })
        .expect("source user node");
    store
        .append_worker(vec![EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new("prompt-source-done"),
            seq: 0,
            session_id: source.clone(),
            branch_id: None,
            run_id: Some(source_run),
            agent_id: None,
            device_id: DeviceId::new("prompt-metafork-device"),
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
            payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
                .expect("done payload"),
        }])
        .await
        .expect("terminalize source");

    let proposal = SessionMetaforkProposal {
        removals: vec![SessionMetaforkRemoval {
            from_seq: user_seq,
            through_seq: user_seq,
            reason: "remove the chocolate discussion".into(),
            preview: Some("secret chocolate ganache ratio 7:3".into()),
            reviewed_events: Vec::new(),
        }],
    };
    let child = SessionId::new("prompt-metafork-child");
    let request_json = serde_json::json!({
        "description": "remove parts about chocolate",
        "proposal": &proposal,
    })
    .to_string();
    let mut command = SessionForkCommand {
        command_id: "prompt-metafork".into(),
        request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
        request_json,
        source_session_id: source,
        session_id: child.clone(),
        worker_generation: store.worker_generation(),
        source_branch_id: None,
        fork_node_id,
        fork_seq,
        name: Some("Chocolate-free".into()),
        metafork: Some(SessionMetaforkCommit {
            description: "remove parts about chocolate".into(),
            model_proposal: proposal,
            accepted_proposal_digest: String::new(),
        }),
        audit_event_id: EventId::new("prompt-metafork-audit"),
        device_id: DeviceId::new("prompt-metafork-device"),
    };
    let metafork = command.metafork.as_ref().expect("metafork command");
    let accepted_proposal_digest = SessionMetaforkReviewManifest {
        command_id: command.command_id.clone(),
        source_session_id: command.source_session_id.clone(),
        worker_generation: command.worker_generation,
        source_branch_id: command.source_branch_id.clone(),
        fork_node_id: command.fork_node_id.clone(),
        fork_seq: command.fork_seq,
        name: command.name.clone(),
        description: metafork.description.clone(),
        model_proposal: metafork.model_proposal.clone(),
    }
    .digest()
    .expect("review digest");
    command
        .metafork
        .as_mut()
        .expect("metafork command")
        .accepted_proposal_digest = accepted_proposal_digest;
    command.request_json = serde_json::json!({
        "description": "remove parts about chocolate",
        "metafork": &command.metafork,
    })
    .to_string();
    command.request_digest = blake3::hash(command.request_json.as_bytes())
        .to_hex()
        .to_string();
    let SessionForkOutcome::Committed { .. } = store.fork_session(command).await.expect("metafork")
    else {
        panic!("fresh metafork commits");
    };

    let raw = StoreHandle::read(&store, &child, 0, 256)
        .await
        .expect("child journal");
    let copied = raw
        .iter()
        .find(|envelope| envelope.payload.to_string().contains("secret chocolate"))
        .expect("content remains in child journal");
    assert_eq!(copied.render.prompt, PromptRender::Omit);

    let messages =
        PromptHistoryCompiler::compile_idle_with_artifacts(&store, &store, &child, None, None)
            .await
            .expect("compile child prompt");
    assert!(!format!("{messages:?}").contains("secret chocolate"));

    store.close().await.expect("close store");
}
