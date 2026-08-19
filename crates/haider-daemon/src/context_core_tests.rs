#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use async_trait::async_trait;
use haider_core::{
    BranchCreateCommand, PromptHistoryCompiler, SessionCreateCommand, SqliteStoreHandle,
    StoreHandle, TurnAcceptCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind, TreeNode,
};
use haider_protocol::ids::{BranchId, DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{
    Block, CacheStatAvailability, FinishReason, NormalizedUsage, Usage, UsageRequestKind,
    UsageSource,
};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

struct FixedProviderFactory {
    provider: Arc<dyn Provider>,
}

struct FixedWindowProviderFactory {
    provider: Arc<dyn Provider>,
    context_window: u64,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: Some(32_000),
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

#[async_trait]
impl ProviderFactory for FixedWindowProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: Some(self.context_window),
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

#[cfg(unix)]
const BRANCH_EXEC_COMMAND: &str = "printf branch";
#[cfg(windows)]
const BRANCH_EXEC_COMMAND: &str = "[Console]::Out.Write('branch')";

// The production Windows interpreter is inbox PowerShell. Its cold process
// startup is materially slower under the fully concurrent per-crate gate;
// this bound covers that platform cost without changing Unix's five-second
// regression budget or any production deadline.
#[cfg(windows)]
const PROCESS_TURN_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(not(windows))]
const PROCESS_TURN_DEADLINE: Duration = Duration::from_secs(5);

/// MUTATION CHECK: leave the selected branch out of worker startup,
/// `HarnessConfig`, or terminal sinks. Expected RUNTIME failure: at least one
/// non-aggregate envelope for `branch-run` below is written on main.
#[tokio::test]
async fn accepted_branch_reaches_worker_history_items_nodes_and_terminal_state() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("branch-worker-propagation");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "main answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitToolCall {
            call_id: "branch-exec".into(),
            name: "process_exec".into(),
            args: serde_json::json!({"command": BRANCH_EXEC_COMMAND}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "branch-exec".into(),
        },
        FakeStep::EmitText {
            text: "branch answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "branch-only summary".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install manager");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-branch-worker".into(),
        request_digest: "create-branch-worker-digest".into(),
        request_json: r#"{"session":"branch-worker"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: Some(haider_protocol::session::SessionPermissionOverridesV1 {
            allow_writes: false,
            allow_exec: true,
            auto_allow: false,
        }),
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-branch-worker"),
        device_id: DeviceId::new("branch-worker-device"),
    })
    .await
    .expect("create session");
    let main_run = RunId::new("main-run");
    let main = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-main-worker".into(),
            request_digest: "submit-main-worker-digest".into(),
            request_json: r#"{"turn":"main"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: main_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "main history".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("main-worker-queued"),
            user_event_id: EventId::new("main-worker-user"),
            active_event_id: EventId::new("main-worker-active"),
            device_id: DeviceId::new("branch-worker-device"),
        })
        .await
        .expect("accept main");
    handle.submit(main).await.expect("submit main");
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(&session_id, 0, 256).await.expect("read");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&main_run)
                    && matches!(
                        serde_json::from_value::<EventPayload>(event.payload.clone()),
                        Ok(EventPayload::RunState(RunState::Done))
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("main completes");
    let journal = store.read(&session_id, 0, 256).await.expect("read main");
    let (fork_node, fork_seq) = journal
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&main_run))
        .filter_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value(event.payload.clone()).ok()?
            else {
                return None;
            };
            Some((node.node, event.seq))
        })
        .next_back()
        .expect("main terminal node");
    let branch_id = BranchId::new("worker-branch");
    let request_json = r#"{"fork":"main-terminal"}"#.to_owned();
    store
        .create_branch(BranchCreateCommand {
            command_id: "create-worker-branch".into(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: fork_node.clone(),
            fork_seq,
            name: Some("Worker branch".into()),
            event_id: EventId::new("created-worker-branch"),
            device_id: DeviceId::new("branch-worker-device"),
        })
        .await
        .expect("create branch");
    let branch_run = RunId::new("branch-run");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-branch-worker".into(),
            request_digest: "submit-branch-worker-digest".into(),
            request_json: r#"{"turn":"branch"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: branch_run.clone(),
            agent_id: None,
            branch_id: Some(branch_id.clone()),
            text: "branch history".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("branch-worker-queued"),
            user_event_id: EventId::new("branch-worker-user"),
            active_event_id: EventId::new("branch-worker-active"),
            device_id: DeviceId::new("branch-worker-device"),
        })
        .await
        .expect("accept branch");
    assert_eq!(accepted.branch_id, Some(branch_id.clone()));
    handle.submit(accepted).await.expect("submit branch");
    timeout(PROCESS_TURN_DEADLINE, async {
        loop {
            let events = store.read(&session_id, 0, 512).await.expect("read");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&branch_run)
                    && matches!(
                        serde_json::from_value::<EventPayload>(event.payload.clone()),
                        Ok(EventPayload::RunState(RunState::Done))
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("branch completes");
    let events = store.read(&session_id, 0, 512).await.expect("read branch");
    let mut saw_user = false;
    let mut saw_item = false;
    let mut saw_node = false;
    let mut saw_done = false;
    let mut saw_effect = false;
    let mut saw_tool_result = false;
    for event in events
        .iter()
        .filter(|event| event.run_id.as_ref() == Some(&branch_run))
    {
        let payload = serde_json::from_value::<EventPayload>(event.payload.clone())
            .expect("typed branch event");
        if matches!(&payload, EventPayload::SessionState(_)) {
            assert_eq!(event.branch_id, None);
            continue;
        }
        assert_eq!(event.branch_id, Some(branch_id.clone()));
        saw_user |= matches!(&payload, EventPayload::UserMessage { .. });
        saw_item |= matches!(&payload, EventPayload::Item(_));
        saw_node |= matches!(&payload, EventPayload::NodeCommitted(_));
        saw_done |= matches!(&payload, EventPayload::RunState(RunState::Done));
        saw_effect |= matches!(&payload, EventPayload::Effect(_));
        saw_tool_result |= matches!(&payload, EventPayload::ToolResult { .. });
    }
    assert!(saw_user && saw_item && saw_node && saw_done && saw_effect && saw_tool_result);

    // MUTATION CHECK: let pending-receipt journal fallback ignore branch, or
    // fail to finalize a deterministically committed compaction node. Expected
    // RUNTIME failure: this replay invokes the provider, returns main/sibling
    // coordinates, or appends another compaction node.
    let committed_command = "committed-branch-compaction";
    let committed_request_json = serde_json::to_string(&serde_json::json!({
        "session_id": &session_id,
        "worker_generation": store.worker_generation(),
        "branch_id": Some(&branch_id),
    }))
    .expect("serialize pending receipt request");
    store
        .claim_context_compaction_receipt(
            committed_command.into(),
            blake3::hash(committed_request_json.as_bytes())
                .to_hex()
                .to_string(),
            committed_request_json,
        )
        .await
        .expect("claim unfinished compaction receipt");
    let branch_descriptor = StoreHandle::branch_lineage(&store, &session_id, Some(&branch_id))
        .await
        .expect("read branch lineage")
        .pop()
        .expect("selected branch descriptor");
    let recovered_operation = format!("manual-{committed_command}");
    let recovered_run = RunId::new(format!("manual-compact-{committed_command}"));
    let recovered_artifact = store
        .put(b"already committed branch summary".to_vec())
        .await
        .expect("put recovered summary");
    let recovered_intent = CompactionIntent {
        operation_id: recovered_operation.clone(),
        covers_from: fork_node.clone(),
        covers_to: branch_descriptor.head_node_id.clone(),
        resume_cause: CompactionResume::ManualIdle,
    };
    let recovered_item = TurnItem::Extension {
        kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
        data: serde_json::to_value(&recovered_intent).expect("serialize recovered intent"),
    };
    let recovered_envelope = |event_id: &str, payload: EventPayload| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: Some(branch_id.clone()),
        run_id: Some(recovered_run.clone()),
        agent_id: None,
        device_id: DeviceId::new("branch-worker-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("recovered payload"),
    };
    let mut recovered_batch = vec![
        recovered_envelope(
            "committed-branch-intent",
            EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new("committed-branch-intent-item"),
                item: recovered_item,
            }),
        ),
        recovered_envelope(
            "committed-branch-node",
            EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new(format!("compaction-node-{recovered_operation}")),
                parent: Some(branch_descriptor.head_node_id),
                kind: NodeKind::Compaction {
                    covers_from: recovered_intent.covers_from.clone(),
                    covers_to: recovered_intent.covers_to.clone(),
                    summary_artifact: recovered_artifact,
                    tokens_before: 100,
                    tokens_after: 8,
                    resume_cause: CompactionResume::ManualIdle,
                },
            }),
        ),
    ];
    StoreHandle::append(&store, &mut recovered_batch)
        .await
        .expect("commit node before receipt finalization");
    let requests_before_recovery = provider.requests().len();
    let recovered_receipt = handle
        .compact(
            session_id.clone(),
            committed_command.into(),
            store.worker_generation(),
            Some(branch_id.clone()),
        )
        .await
        .expect("finalize committed branch compaction");
    assert_eq!(recovered_receipt.run_id, recovered_run);
    assert_eq!(recovered_receipt.accepted_seq, recovered_batch[0].seq);
    assert_eq!(recovered_receipt.branch_id, Some(branch_id.clone()));
    assert_eq!(provider.requests().len(), requests_before_recovery);
    let recovered_replay = handle
        .compact(
            session_id.clone(),
            committed_command.into(),
            store.worker_generation(),
            Some(branch_id.clone()),
        )
        .await
        .expect("replay finalized branch compaction");
    assert_eq!(recovered_replay, recovered_receipt);
    assert_eq!(provider.requests().len(), requests_before_recovery);

    let compacted = handle
        .compact(
            session_id.clone(),
            "compact-worker-branch".into(),
            store.worker_generation(),
            Some(branch_id.clone()),
        )
        .await
        .expect("compact selected branch");
    assert_eq!(compacted.branch_id, Some(branch_id.clone()));
    let after_compaction = store
        .read(&session_id, 0, 768)
        .await
        .expect("read compaction");
    assert!(after_compaction.iter().any(|event| {
        event.run_id.as_ref() == Some(&compacted.run_id)
            && event.branch_id.as_ref() == Some(&branch_id)
            && matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::NodeCommitted(ref node))
                    if matches!(node.kind, NodeKind::Compaction { .. })
            )
    }));
    let branch_prompt = PromptHistoryCompiler::compile_idle_with_artifacts(
        &store,
        &store,
        &session_id,
        Some(&branch_id),
        None,
    )
    .await
    .expect("compile compacted branch");
    let main_prompt =
        PromptHistoryCompiler::compile_idle_with_artifacts(&store, &store, &session_id, None, None)
            .await
            .expect("compile uncompacted main");
    let prompt_text = |messages: &[haider_provider::Message]| {
        messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(prompt_text(&branch_prompt).contains("branch-only summary"));
    assert!(!prompt_text(&main_prompt).contains("branch-only summary"));
    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: hard-code automatic compaction planning or emission to
/// main. Expected RUNTIME failure: the oversized branch turn either compacts
/// the wrong scope or its summary/node is emitted without branch A.
#[tokio::test]
async fn automatic_compaction_plans_and_commits_on_the_accepted_branch() {
    let root = tempfile::tempdir().expect("profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("auto-compaction-branch-session");
    let source_run = RunId::new("auto-compaction-source-run");
    let branch_run = RunId::new("auto-compaction-branch-run");
    let branch_id = BranchId::new("auto-compaction-branch-a");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "automatic branch summary".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "answer after automatic compaction".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedWindowProviderFactory {
                provider: provider.clone(),
                context_window: 12_000,
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let handle = manager.handle();
    hub.install_worker_manager(handle.clone())
        .expect("install manager");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-auto-compaction-branch".into(),
        request_digest: "create-auto-compaction-branch-digest".into(),
        request_json: r#"{"session":"auto-compaction-branch"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 64,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-auto-compaction-branch"),
        device_id: DeviceId::new("auto-compaction-device"),
    })
    .await
    .expect("create session");
    let large_source = "AUTO_BRANCH_SOURCE ".repeat(4_000);
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "accept-auto-compaction-source".into(),
        request_digest: "accept-auto-compaction-source-digest".into(),
        request_json: r#"{"turn":"auto-compaction-source"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: store.worker_generation(),
        run_id: source_run.clone(),
        agent_id: None,
        branch_id: None,
        text: large_source.clone(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new("auto-compaction-source-queued"),
        user_event_id: EventId::new("auto-compaction-source-user"),
        active_event_id: EventId::new("auto-compaction-source-active"),
        device_id: DeviceId::new("auto-compaction-device"),
    })
    .await
    .expect("accept source");
    let mut source_done = [EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("auto-compaction-source-done"),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(source_run.clone()),
        agent_id: None,
        device_id: DeviceId::new("auto-compaction-device"),
        authority_epoch: 0,
        worker_generation: store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(EventPayload::RunState(RunState::Done))
            .expect("done payload"),
    }];
    hub.append(&mut source_done)
        .await
        .expect("terminalize source");
    let source_events = store.read(&session_id, 0, 64).await.expect("source events");
    let (fork_node, fork_seq) = source_events
        .iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            (event.run_id.as_ref() == Some(&source_run)).then_some((node.node, event.seq))
        })
        .expect("source fork node");
    let branch_request = r#"{"fork":"auto-compaction-source"}"#.to_owned();
    store
        .create_branch(BranchCreateCommand {
            command_id: "create-auto-compaction-ref".into(),
            request_digest: blake3::hash(branch_request.as_bytes()).to_hex().to_string(),
            request_json: branch_request,
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: fork_node,
            fork_seq,
            name: Some("Automatic compaction A".into()),
            event_id: EventId::new("created-auto-compaction-ref"),
            device_id: DeviceId::new("auto-compaction-device"),
        })
        .await
        .expect("create branch");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "accept-auto-compaction-branch-turn".into(),
            request_digest: "accept-auto-compaction-branch-turn-digest".into(),
            request_json: r#"{"turn":"auto-compaction-branch"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: branch_run.clone(),
            agent_id: None,
            branch_id: Some(branch_id.clone()),
            text: "continue after the large source".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("auto-compaction-branch-queued"),
            user_event_id: EventId::new("auto-compaction-branch-user"),
            active_event_id: EventId::new("auto-compaction-branch-active"),
            device_id: DeviceId::new("auto-compaction-device"),
        })
        .await
        .expect("accept branch turn");
    handle.submit(accepted).await.expect("submit branch turn");
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(&session_id, 0, 256).await.expect("read events");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&branch_run)
                    && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                        |payload| matches!(payload, EventPayload::RunState(RunState::Done)),
                    )
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("branch turn completes");

    let events = store.read(&session_id, 0, 256).await.expect("final events");
    let compactions = events
        .iter()
        .filter_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload.clone()).ok()?
            else {
                return None;
            };
            matches!(
                node.kind,
                NodeKind::Compaction {
                    resume_cause: CompactionResume::AutoMidTurn,
                    ..
                }
            )
            .then_some(event)
        })
        .collect::<Vec<_>>();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].run_id, Some(branch_run));
    assert_eq!(compactions[0].branch_id, Some(branch_id));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let resumed_text = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(resumed_text.contains("automatic branch summary"));
    assert!(!resumed_text.contains("AUTO_BRANCH_SOURCE"));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// MUTATION CHECK: skip durable command lookup before manual compaction.
/// Expected runtime failure: the provider sees a third request or a second
/// compaction intent/node is committed for the replayed command id.
///
/// MUTATION CHECK: bypass the global command-receipt claim. Expected runtime
/// failure: a turn-submit command id is accepted again for compaction.
///
/// MUTATION CHECK: omit the manual reset footprint, select a stale pre-turn
/// snapshot, or account only summary text. Expected runtime failure: the
/// latest footprint is not after the compaction node or does not exceed the
/// node's summary-only token count with compiled request overhead included.
#[tokio::test]
async fn cm1f_manual_compaction_usage_is_journaled_once_in_its_own_lane() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let session_id = SessionId::new("manual-compaction-replay");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "durable answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        // The live codex responses-lite stream sends opaque reasoning
        // fragments on EVERY turn — the summarizer must ignore them
        // (probe autopsy: rejecting them failed 100% of live
        // compactions on openai-oauth).
        // MUTATION CHECK: reject ProviderOpaque in the summarizer loop —
        // this test fails with "unsupported structured output"-era
        // behavior.
        FakeStep::EmitProviderOpaque {
            provider: "openai".into(),
            data: serde_json::json!({"reasoning": "opaque-frame"}),
        },
        FakeStep::EmitText {
            text: "summary of durable history".into(),
        },
        FakeStep::EmitUsage {
            usage: Usage {
                input: 100,
                output: 10,
                reasoning: 0,
                cached: 75,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
                normalized: Some(NormalizedUsage {
                    logical_input: 100,
                    uncached_input: 25,
                    cache_read_input: 75,
                    billed_output: 10,
                    cache_status: CacheStatAvailability::Present,
                    cache_telemetry_input: 100,
                    ..NormalizedUsage::default()
                }),
                scope: None,
                cache_cost: None,
            },
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let manager_handle = manager.handle();
    hub.install_worker_manager(manager_handle.clone())
        .expect("install worker manager");
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-manual-compaction".into(),
        request_digest: "create-manual-compaction-digest".into(),
        request_json: r#"{"session":"manual-compaction"}"#.into(),
        session_id: session_id.clone(),
        cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned(),
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-manual-compaction"),
        device_id: DeviceId::new("manual-compaction-device"),
    })
    .await
    .expect("create session");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-before-manual-compaction".into(),
            request_digest: "submit-before-manual-compaction-digest".into(),
            request_json: r#"{"turn":"before-manual-compaction"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: store.worker_generation(),
            run_id: haider_protocol::ids::RunId::new("before-manual-compaction"),
            agent_id: None,
            branch_id: None,
            text: "build durable history".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("manual-compaction-queued"),
            user_event_id: EventId::new("manual-compaction-user"),
            active_event_id: EventId::new("manual-compaction-active"),
            device_id: DeviceId::new("manual-compaction-device"),
        })
        .await
        .expect("accept seed turn");
    manager_handle
        .submit(accepted)
        .await
        .expect("submit seed turn");
    timeout(Duration::from_secs(5), async {
        loop {
            let events = store.read(&session_id, 0, 512).await.expect("read events");
            if events.iter().any(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone())
                    .is_ok_and(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("seed turn completes");

    // The journal's Done fact lands a beat before the worker's in-memory
    // turn slot releases; under parallel-suite load the reused-id compact
    // can race that gap and answer Busy first. Bounded retry through the
    // Busy window (gate27 hygiene class) so the assert exercises the
    // receipt-identity law, not the settle race.
    let reused = {
        let mut attempt = 0;
        loop {
            let error = manager_handle
                .compact(
                    session_id.clone(),
                    "submit-before-manual-compaction".into(),
                    store.worker_generation(),
                    None,
                )
                .await
                .expect_err("turn command id cannot be reused for compaction");
            if error.code == haider_protocol::error::ErrorCode::Busy && attempt < 40 {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            break error;
        }
    };
    assert_eq!(
        reused.code,
        haider_protocol::error::ErrorCode::InvalidArgument
    );
    assert_eq!(provider.requests().len(), 1);

    let first = manager_handle
        .compact(
            session_id.clone(),
            "same-compact-command".into(),
            store.worker_generation(),
            None,
        )
        .await
        .expect("first compaction");
    let replay = manager_handle
        .compact(
            session_id.clone(),
            "same-compact-command".into(),
            store.worker_generation(),
            None,
        )
        .await
        .expect("compaction replay");
    assert_eq!(first, replay);
    assert_eq!(provider.requests().len(), 2);

    let journal = store
        .read(&session_id, 0, 512)
        .await
        .expect("read compacted journal");
    let payloads = journal
        .into_iter()
        .filter_map(|event| {
            serde_json::from_value::<EventPayload>(event.payload)
                .ok()
                .map(|payload| (event.seq, payload))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        payloads
            .iter()
            .filter(|(_, payload)| matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { kind, .. },
                    ..
                }) if kind == COMPACTION_INTENT_EXTENSION_KIND
            ))
            .count(),
        1
    );
    let compaction_usage = payloads
        .iter()
        .filter_map(|(_, payload)| match payload {
            EventPayload::Usage(usage)
                if usage
                    .scope
                    .as_ref()
                    .is_some_and(|scope| scope.request_kind == UsageRequestKind::Compaction) =>
            {
                Some(usage)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        compaction_usage.len(),
        1,
        "MUTATION CHECK: discard UsageUpdate in the compactor and this lane disappears"
    );
    assert_eq!(
        compaction_usage[0]
            .normalized
            .as_ref()
            .expect("normalized compaction usage")
            .cache_read_input,
        75
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|(_, payload)| matches!(
                payload,
                EventPayload::NodeCommitted(node)
                    if matches!(node.kind, NodeKind::Compaction { .. })
            ))
            .count(),
        1
    );
    let (compaction_seq, summary_only_tokens) = payloads
        .iter()
        .find_map(|(seq, payload)| match payload {
            EventPayload::NodeCommitted(node) => match &node.kind {
                NodeKind::Compaction { tokens_after, .. } => Some((*seq, *tokens_after)),
                _ => None,
            },
            _ => None,
        })
        .expect("manual compaction node");
    let (reset_seq, reset) = payloads
        .iter()
        .filter_map(|(seq, payload)| match payload {
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                ContextFootprint::from_extension_item(item).map(|footprint| (*seq, footprint))
            }
            _ => None,
        })
        .next_back()
        .expect("manual compaction reset footprint");
    assert!(reset_seq > compaction_seq);
    assert_eq!(reset.truth, ContextFootprintTruth::Estimated);
    assert_eq!(reset.context_window, Some(32_000));
    assert!(reset.used_tokens > summary_only_tokens);
    assert!(reset.used_tokens < 32_000);

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");

    // Lost-response replay remains unfenced after a daemon generation
    // change and returns the original durable coordinates without invoking a
    // new summarizer.
    // Bounded StoreLocked retry: drop() can return before the profile
    // lock fully releases under parallel suite load (gate27 hygiene
    // precedent, fourth fixture in this class).
    let reopened = {
        let mut attempt = 0;
        loop {
            match SqliteStoreHandle::open(root.path()).await {
                Ok(store) => break store,
                Err(error) if error.retryable && attempt < 40 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(error) => panic!("reopen store: {error:?}"),
            }
        }
    };
    let replay_provider = Arc::new(FakeProvider::new(Vec::new()));
    let replay_hub =
        SessionHub::new(reopened.clone(), SessionHubConfig::default()).expect("replay hub");
    let replay_manager = WorkerManager::start(
        replay_hub.clone(),
        WorkerDependencies {
            provider_factory: Arc::new(FixedProviderFactory {
                provider: replay_provider.clone(),
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    let replay_handle = replay_manager.handle();
    replay_hub
        .install_worker_manager(replay_handle.clone())
        .expect("install replay manager");
    let after_restart = replay_handle
        .compact(
            session_id,
            "same-compact-command".into(),
            first.worker_generation,
            None,
        )
        .await
        .expect("cross-generation receipt replay");
    assert_eq!(after_restart, first);
    assert!(replay_provider.requests().is_empty());
    replay_manager
        .shutdown()
        .await
        .expect("replay manager shutdown");
    replay_hub.shutdown().await.expect("replay hub shutdown");
    reopened.close().await.expect("reopened store close");
}

/// MUTATION CHECK: omit the durable intent before entering Compacting.
/// Expected runtime failure: the recovered journal has no completed intent,
/// violating the asserted recoverable operation seam.
///
/// MUTATION CHECK: make the intent itself activate substitution. Expected
/// runtime failure: the prompt after reopen differs from the original even
/// though no compaction node was ever committed.
#[tokio::test]
async fn crash_after_compaction_intent_abandons_without_changing_the_prompt() {
    use crate::turn_recovery::recover_interrupted_turns;

    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let generation = store.worker_generation();
    let session_id = SessionId::new("compaction-intent-crash");
    let source_run = RunId::new("compaction-source-run");
    let compaction_run = RunId::new("crashed-compaction-run");
    let device_id = DeviceId::new("compaction-crash-device");
    store
        .create_session(SessionCreateCommand {
            command_id: "create-compaction-crash".into(),
            request_digest: "create-compaction-crash-digest".into(),
            request_json: r#"{"session":"compaction-crash"}"#.into(),
            session_id: session_id.clone(),
            cwd: std::fs::canonicalize(std::env::current_dir().expect("cwd"))
                .expect("canonical cwd")
                .to_string_lossy()
                .into_owned(),
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new("created-compaction-crash"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
    store
        .accept_turn(TurnAcceptCommand {
            command_id: "accept-compaction-source".into(),
            request_digest: "accept-compaction-source-digest".into(),
            request_json: r#"{"turn":"source"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: source_run.clone(),
            agent_id: None,
            branch_id: None,
            text: "history that must survive".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("compaction-source-queued"),
            user_event_id: EventId::new("compaction-source-user"),
            active_event_id: EventId::new("compaction-source-active"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept source");

    let envelope = |event: &str,
                    run_id: &RunId,
                    branch_id: Option<&BranchId>,
                    payload: EventPayload| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: branch_id.cloned(),
        run_id: Some(run_id.clone()),
        agent_id: None,
        device_id: device_id.clone(),
        authority_epoch: 0,
        worker_generation: generation,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 0,
        render: RenderTargets {
            ui: false,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload: serde_json::to_value(payload).expect("payload"),
    };
    let mut source_done = [envelope(
        "compaction-source-done",
        &source_run,
        None,
        EventPayload::RunState(RunState::Done),
    )];
    StoreHandle::append(&store, &mut source_done)
        .await
        .expect("finish source history");
    let source_node = NodeId::new("node-compaction-source-user");
    let source_seq = store
        .read(&session_id, 0, 64)
        .await
        .expect("read source node")
        .into_iter()
        .find_map(|event| {
            let EventPayload::NodeCommitted(node) =
                serde_json::from_value::<EventPayload>(event.payload).ok()?
            else {
                return None;
            };
            (node.node == source_node).then_some(event.seq)
        })
        .expect("source node sequence");
    let branch_id = BranchId::new("compaction-crash-branch");
    let branch_request = r#"{"fork":"compaction-source"}"#.to_owned();
    store
        .create_branch(BranchCreateCommand {
            command_id: "create-compaction-crash-branch".into(),
            request_digest: blake3::hash(branch_request.as_bytes()).to_hex().to_string(),
            request_json: branch_request,
            session_id: session_id.clone(),
            worker_generation: generation,
            branch_id: branch_id.clone(),
            source_branch_id: None,
            fork_node_id: source_node.clone(),
            fork_seq: source_seq,
            name: None,
            event_id: EventId::new("created-compaction-crash-branch"),
            device_id: device_id.clone(),
        })
        .await
        .expect("create crash branch");
    let intent = CompactionIntent {
        operation_id: "crashed-operation".into(),
        covers_from: source_node.clone(),
        covers_to: source_node,
        resume_cause: CompactionResume::ManualIdle,
    };
    let item = TurnItem::Extension {
        kind: COMPACTION_INTENT_EXTENSION_KIND.into(),
        data: serde_json::to_value(intent).expect("intent"),
    };
    let item_id = ItemId::new("crashed-compaction-intent");
    let mut crash_batch = vec![
        envelope(
            "crashed-intent-started",
            &compaction_run,
            Some(&branch_id),
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: item.clone(),
            }),
        ),
        envelope(
            "crashed-intent-completed",
            &compaction_run,
            Some(&branch_id),
            EventPayload::Item(ItemEvent::Completed { item_id, item }),
        ),
        envelope(
            "crashed-compacting",
            &compaction_run,
            Some(&branch_id),
            EventPayload::RunState(RunState::Compacting),
        ),
    ];
    StoreHandle::append(&store, &mut crash_batch)
        .await
        .expect("append durable crash boundary");
    let before = PromptHistoryCompiler::compile_with_artifacts(
        &store,
        &store,
        &session_id,
        Some(&branch_id),
        None,
        &source_run,
    )
    .await
    .expect("prompt before restart");
    assert!(!before.is_empty());
    let before = serde_json::to_vec(&before).expect("serialize prompt");
    store.close().await.expect("close before recovery");

    let recovered = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
    assert!(
        recover_interrupted_turns(&recovered, &device_id)
            .await
            .expect("recover interrupted compaction")
            .is_empty()
    );
    let after = PromptHistoryCompiler::compile_with_artifacts(
        &recovered,
        &recovered,
        &session_id,
        Some(&branch_id),
        None,
        &source_run,
    )
    .await
    .expect("prompt after recovery");
    assert_eq!(
        serde_json::to_vec(&after).expect("serialize prompt"),
        before
    );

    let recovered_events = recovered
        .read(&session_id, 0, 256)
        .await
        .expect("read recovered journal");
    let payloads = recovered_events
        .iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
        .collect::<Vec<_>>();
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::Extension { kind, .. },
            ..
        }) if kind == COMPACTION_INTENT_EXTENSION_KIND
    )));
    assert!(!payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::NodeCommitted(node) if matches!(node.kind, NodeKind::Compaction { .. })
    )));
    assert!(
        payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Errored)))
    );
    assert!(recovered_events.iter().any(|event| {
        event.run_id.as_ref() == Some(&compaction_run)
            && event.branch_id.as_ref() == Some(&branch_id)
            && matches!(
                serde_json::from_value::<EventPayload>(event.payload.clone()),
                Ok(EventPayload::RunState(RunState::Errored))
            )
    }));
    assert!(recovered_events.iter().all(|event| {
        !matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::SessionState(_))
        ) || event.branch_id.is_none()
    }));
    recovered.close().await.expect("close recovered store");
}
