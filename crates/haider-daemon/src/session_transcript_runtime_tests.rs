#![allow(clippy::expect_used)]
use super::*;
use crate::session_hub::SessionHubConfig;
use haider_core::{
    SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::provider::FinishReason;
use haider_protocol::session::SessionMetadataV1;
use haider_provider::{FakeProvider, FakeStep, Provider};
use tokio::time::{Duration, timeout};

struct FixedProviderFactory {
    provider: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            account_incarnation: None,
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
            compaction_promotion: None,
        })
    }
}

struct World {
    store: SqliteStoreHandle,
    hub: SessionHub,
    session_id: SessionId,
    device_id: DeviceId,
    manager: WorkerManager,
    _root: tempfile::TempDir,
}
impl World {
    async fn boot_with_scope(
        prefix: &str,
        provider: Arc<FakeProvider>,
        tools: Vec<String>,
        allow_exec: bool,
    ) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(FixedProviderFactory { provider }),
                // Start with the economy pack; the script must discover the tool.
                tool_factory: crate::worker::DaemonDependencies::default()
                    .with_tool_exposure(Some(tools))
                    .tool_factory,
                delegation: None,
                web_search: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("install manager");
        let session_id = SessionId::new(format!("{prefix}-session"));
        let device_id = DeviceId::new(format!("{prefix}-device"));
        #[cfg(not(feature = "android-standalone"))]
        let workspace = std::env::current_dir().expect("cwd");
        #[cfg(feature = "android-standalone")]
        let workspace = crate::android_workspace::test_root();
        let cwd = std::fs::canonicalize(workspace)
            .expect("canonical cwd")
            .to_string_lossy()
            .into_owned();
        hub.create_internal_session(SessionCreateCommand {
            command_id: format!("create-{prefix}"),
            request_digest: format!("create-{prefix}-digest"),
            request_json: format!(r#"{{"session":"{prefix}"}}"#),
            session_id: session_id.clone(),
            cwd,
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            max_tokens_source: None,
            permission_overrides: allow_exec.then_some(
                haider_protocol::session::SessionPermissionOverridesV1 {
                    allow_exec: true,
                    ..Default::default()
                },
            ),
            effort: None,
            fast: false,
            cache_policy: Default::default(),
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(format!("created-{prefix}")),
            device_id: device_id.clone(),
        })
        .await
        .expect("create session");
        Self {
            store,
            hub,
            session_id,
            device_id,
            manager,
            _root: root,
        }
    }

    async fn run_turn(&self, label: &str, text: &str) -> RunId {
        let run_id = RunId::new(format!("{label}-run"));
        let accepted = self
            .hub
            .accept_internal_turn(TurnAcceptCommand {
                command_id: format!("submit-{label}"),
                request_digest: format!("submit-{label}-digest"),
                request_json: format!(r#"{{"turn":"{label}"}}"#),
                session_id: self.session_id.clone(),
                worker_generation: self.store.worker_generation(),
                run_id: run_id.clone(),
                agent_id: None,
                branch_id: None,
                text: text.into(),
                attachments: Vec::new(),
                mode: DeliveryMode::Steer,
                queued_event_id: EventId::new(format!("{label}-queued")),
                user_event_id: EventId::new(format!("{label}-user")),
                active_event_id: EventId::new(format!("{label}-active")),
                device_id: self.device_id.clone(),
            })
            .await
            .expect("accept turn");
        assert_eq!(accepted.disposition, TurnAdmissionDisposition::Started);
        self.manager
            .handle()
            .submit(accepted)
            .await
            .expect("submit turn");
        self.await_done(&self.session_id.clone(), &run_id).await;
        run_id
    }

    async fn await_done(&self, session_id: &SessionId, run_id: &RunId) {
        timeout(Duration::from_secs(10), async {
            loop {
                let events = self
                    .store
                    .read(session_id, 0, 2048)
                    .await
                    .expect("read journal");
                if events.iter().any(|event| {
                    event.run_id.as_ref() == Some(run_id)
                        && event.payload.decode_event().is_ok_and(|payload| {
                            matches!(payload, EventPayload::RunState(RunState::Done))
                        })
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("run reaches Done");
    }

    async fn close(self) {
        self.manager.shutdown().await.expect("manager shutdown");
        self.hub.shutdown().await.expect("hub shutdown");
        self.store.close().await.expect("store close");
    }
}

fn call(call_id: &str, name: &str, args: serde_json::Value) -> [FakeStep; 3] {
    [
        FakeStep::EmitToolCall {
            call_id: call_id.into(),
            name: name.into(),
            args,
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
    ]
}

#[tokio::test]
async fn session_transcript_discovery_read_and_denial_are_durable_tool_receipts() {
    let foreign_provider = Arc::new(FakeProvider::new(Vec::new()));
    let foreign = World::boot_with_scope("foreign", foreign_provider, vec![], false).await;
    let mut script = Vec::new();
    script.extend(call(
        "discover",
        "list_tools",
        serde_json::json!({"filter":"session_transcript"}),
    ));
    script.extend(call(
        "read",
        "session_transcript",
        serde_json::json!({"session_id":"source-session","limit":1024}),
    ));
    script.extend(call(
        "foreign",
        "session_transcript",
        serde_json::json!({"session_id":foreign.session_id.as_str()}),
    ));
    script.push(FakeStep::EmitText {
        text: "handoff inspected".into(),
    });
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let provider = Arc::new(FakeProvider::new(script));
    let world = World::boot_with_scope("handoff", provider, vec![], false).await;
    let source = SessionId::new("source-session");
    let mut source_events = [RawEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new("source-user"),
        seq: 0,
        session_id: source.clone(),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: world.device_id.clone(),
        authority_epoch: 0,
        worker_generation: world.store.worker_generation(),
        causation_id: None,
        correlation_id: None,
        committed_at_ms: 1,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Verbatim,
        },
        payload: serde_json::to_value(EventPayload::UserMessage {
            text: "prior observation [REDACTED]".into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Steer,
        })
        .expect("source payload")
        .into(),
    }];
    world
        .store
        .append(&mut source_events)
        .await
        .expect("source journal");
    world.run_turn("handoff", "read the prior session").await;
    assert_eq!(
        world
            .store
            .read(&source, 0, 100)
            .await
            .expect("unchanged source"),
        source_events
    );
    let events = world
        .store
        .read(&world.session_id, 0, 2048)
        .await
        .expect("receipts");
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match event.payload.decode_event().ok()? {
            EventPayload::ToolResult { call_id, result } => Some((call_id, result)),
            _ => None,
        })
        .collect();
    let read = &results
        .iter()
        .find(|(id, _)| id == "read")
        .expect("read receipt")
        .1;
    assert_eq!(read.status, ToolResultStatus::Completed);
    assert!(
        read.preview.contains("user: prior observation [REDACTED]"),
        "{}",
        read.preview
    );
    assert!(read.preview.contains("next_after_seq="));
    let denied = &results
        .iter()
        .find(|(id, _)| id == "foreign")
        .expect("denial receipt")
        .1;
    assert_eq!(denied.status, ToolResultStatus::Rejected);
    assert!(denied.preview.contains("current profile"));
    let denial: serde_json::Value =
        serde_json::from_str(&denied.preview).expect("typed permission error");
    assert_eq!(denial["status"], "denied");
    assert_eq!(denial["error"]["kind"], "permission_denied");
    let discovered = &results
        .iter()
        .find(|(id, _)| id == "discover")
        .expect("discovery receipt")
        .1;
    assert!(discovered.preview.contains("session_transcript"));
    let head = world
        .store
        .latest_seq(&world.session_id)
        .await
        .expect("head");
    let mut cursor = 0;
    let mut all = Vec::new();
    while cursor < head {
        let (page_head, page) = world
            .hub
            .read_session_journal(&world.session_id, cursor, 2)
            .await
            .expect("read")
            .expect("same profile");
        assert_eq!(page_head, head);
        assert!(page.len() <= 2);
        assert_eq!(page[0].seq, cursor + 1);
        cursor = page.last().expect("progress").seq;
        all.extend(page);
    }
    assert_eq!(all, events);
    assert!(
        world
            .hub
            .read_session_journal(&foreign.session_id, 0, 100)
            .await
            .expect("foreign lookup")
            .is_none()
    );
    foreign.close().await;
    world.close().await;
}
