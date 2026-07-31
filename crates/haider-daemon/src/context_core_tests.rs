#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use async_trait::async_trait;
use haider_core::{
    PromptHistoryCompiler, SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CompactionIntent, CompactionResume, NodeKind,
};
use haider_protocol::ids::{DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::FinishReason;
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

struct FixedProviderFactory {
    provider: Arc<dyn Provider>,
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
        })
    }
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
async fn manual_compaction_command_replay_compacts_exactly_once() {
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
        FakeStep::EmitText {
            text: "summary of durable history".into(),
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

    let reused = manager_handle
        .compact(
            session_id.clone(),
            "submit-before-manual-compaction".into(),
            store.worker_generation(),
        )
        .await
        .expect_err("turn command id cannot be reused for compaction");
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
        )
        .await
        .expect("first compaction");
    let replay = manager_handle
        .compact(
            session_id.clone(),
            "same-compact-command".into(),
            store.worker_generation(),
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
    let reopened = SqliteStoreHandle::open(root.path())
        .await
        .expect("reopen store");
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

    let envelope = |event: &str, run_id: &RunId, payload: EventPayload| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
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
        EventPayload::RunState(RunState::Done),
    )];
    StoreHandle::append(&store, &mut source_done)
        .await
        .expect("finish source history");
    let source_node = NodeId::new("node-compaction-source-user");
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
            EventPayload::Item(ItemEvent::Started {
                item_id: item_id.clone(),
                item: item.clone(),
            }),
        ),
        envelope(
            "crashed-intent-completed",
            &compaction_run,
            EventPayload::Item(ItemEvent::Completed { item_id, item }),
        ),
        envelope(
            "crashed-compacting",
            &compaction_run,
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
        None,
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
        None,
        None,
        &source_run,
    )
    .await
    .expect("prompt after recovery");
    assert_eq!(
        serde_json::to_vec(&after).expect("serialize prompt"),
        before
    );

    let payloads = recovered
        .read(&session_id, 0, 256)
        .await
        .expect("read recovered journal")
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
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
    recovered.close().await.expect("close recovered store");
}
