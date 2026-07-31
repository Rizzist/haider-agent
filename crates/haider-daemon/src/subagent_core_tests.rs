#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use async_trait::async_trait;
use haider_core::{
    SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectOutcome, EffectPhase};
use haider_protocol::ids::{DeviceId, EventId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{
    FakeProvider, FakeStep, Provider, ProviderError, ProviderStream, TurnRequest,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{Duration, timeout};

struct InspectingProvider {
    inner: FakeProvider,
    store: SqliteStoreHandle,
    parent_session: SessionId,
    outcome_preceded_child: AtomicBool,
}

#[async_trait]
impl Provider for InspectingProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        self.inner.capabilities().await
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let is_child = request.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, Block::Text { text } if text.starts_with("Delegated task:"))
            })
        });
        if is_child {
            let mut cursor = 0;
            let mut spawn_terminal = false;
            let mut observed = Vec::new();
            loop {
                let page = StoreHandle::read(&self.store, &self.parent_session, cursor, 256)
                    .await
                    .expect("read parent effect journal");
                if page.is_empty() {
                    break;
                }
                cursor = page.last().map_or(cursor, |event| event.seq);
                spawn_terminal |= page.into_iter().any(|event| {
                    observed.push(event.payload.clone());
                    serde_json::from_value::<EventPayload>(event.payload).is_ok_and(|payload| {
                        matches!(
                            payload,
                            EventPayload::Effect(EffectPhase::Outcome {
                                outcome: EffectOutcome::Ok,
                                ..
                            })
                        )
                    })
                });
            }
            assert!(spawn_terminal, "child started before outcome: {observed:?}");
            self.outcome_preceded_child
                .store(spawn_terminal, Ordering::SeqCst);
        }
        self.inner.stream_turn(request).await
    }
}

struct FixedProviderFactory {
    provider: Arc<InspectingProvider>,
}

#[async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, haider_protocol::error::HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.provider.clone(),
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
}

/// MUTATION CHECK: submit the child before terminalizing AgentSpawn, keep the
/// tool effect open for the child's lifetime, skip Waiting(LocalChild), or
/// resume without the report. Expected runtime failure: the child provider
/// observes no spawn outcome, the parent state chain is wrong, or its second
/// request lacks `child report`.
#[tokio::test]
async fn production_spawn_effect_wait_and_report_chain_is_end_to_end() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let parent_session = SessionId::new("w6a-parent-session");
    let provider = Arc::new(InspectingProvider {
        inner: FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: "spawn-call".into(),
                name: "spawn_subagent".into(),
                args: serde_json::json!({
                    "task": "tests",
                    "prompt": "run the focused test suite"
                }),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::EmitText {
                text: "child report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
            FakeStep::ExpectToolResult {
                call_id: "spawn-call".into(),
            },
            FakeStep::EmitText {
                text: "parent merged report".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]),
        store: store.clone(),
        parent_session: parent_session.clone(),
        outcome_preceded_child: AtomicBool::new(false),
    });
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
        .expect("install manager");
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-w6a-parent".into(),
        request_digest: "create-w6a-parent-digest".into(),
        request_json: r#"{"session":"w6a-parent"}"#.into(),
        session_id: parent_session.clone(),
        cwd,
        provider: "fake".into(),
        model: "fake-model".into(),
        max_tokens: 4096,
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-w6a-parent"),
        device_id: DeviceId::new("w6a-test-device"),
    })
    .await
    .expect("create parent");
    let accepted = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "submit-w6a-parent".into(),
            request_digest: "submit-w6a-parent-digest".into(),
            request_json: r#"{"turn":"w6a-parent"}"#.into(),
            session_id: parent_session.clone(),
            worker_generation: store.worker_generation(),
            run_id: haider_protocol::ids::RunId::new("w6a-parent-run"),
            agent_id: None,
            text: "delegate the tests".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
            queued_event_id: EventId::new("w6a-parent-queued"),
            user_event_id: EventId::new("w6a-parent-user"),
            active_event_id: EventId::new("w6a-parent-active"),
            device_id: DeviceId::new("w6a-test-device"),
        })
        .await
        .expect("accept parent");
    assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
    manager_handle
        .submit(accepted)
        .await
        .expect("submit parent worker");

    timeout(Duration::from_secs(10), async {
        loop {
            let events = store
                .read(&parent_session, 0, 512)
                .await
                .expect("read parent");
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
    .expect("parent completes");

    let requests = provider.inner.requests();
    assert!(
        provider.outcome_preceded_child.load(Ordering::SeqCst),
        "spawn outcome must commit before child provider work: {requests:?}"
    );
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .tools
            .iter()
            .all(|tool| tool.name != "spawn_subagent"),
        "W6a children are nonrecursive"
    );
    assert!(requests[2].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { call_id, preview, .. }
                    if call_id == "spawn-call" && preview == "child report"
            )
        })
    }));
    let parent_events = store.read(&parent_session, 0, 512).await.expect("parent");
    let payloads = parent_events
        .iter()
        .map(|event| {
            serde_json::from_value::<EventPayload>(event.payload.clone()).expect("payload")
        })
        .collect::<Vec<_>>();
    let waiting = payloads
        .iter()
        .position(|payload| {
            matches!(
                payload,
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::LocalChild
                })
            )
        })
        .expect("parent waited");
    let resumed = payloads
        .iter()
        .enumerate()
        .skip(waiting + 1)
        .find_map(|(index, payload)| {
            matches!(payload, EventPayload::RunState(RunState::Thinking)).then_some(index)
        })
        .expect("parent resumed");
    assert!(
        !payloads[waiting + 1..resumed]
            .iter()
            .any(|payload| matches!(payload, EventPayload::SessionState(_)))
    );
    assert!(payloads.iter().any(|payload| {
        matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ChildResult { report },
                ..
            }) if report.summary == "child report"
        )
    }));
    let spawned = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::AgentSpawned(manifest) => Some(manifest.clone()),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(spawned.task, "tests");
    let delegation = hub
        .delegation(spawned.agent.clone())
        .await
        .expect("delegation lookup")
        .expect("delegation row");
    let child_events = store
        .read(&delegation.child_session_id, 0, 512)
        .await
        .expect("child events");
    assert!(child_events.iter().all(|event| {
        event.run_id.as_ref() != Some(&delegation.child_run_id)
            || event.agent_id.as_ref() == Some(&spawned.agent)
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}
