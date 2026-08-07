//! W-B daemon-boundary web laws: the LOCAL `web_fetch` tool end to end
//! through a live daemon run (broker journal + typed refusal results, LW6/
//! LW7 daemon halves) and the per-pair advertisement seam (LW8 half).
//! Loopback mock servers only — nothing here dials the real network.

#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, TurnToolFactory,
    WebCapabilityDegrade, WorkerDependencies, WorkerManager, advertised_tool_definitions,
};
use haider_core::{
    SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, Provider};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
}

struct WebWorld {
    store: SqliteStoreHandle,
    hub: SessionHub,
    session_id: SessionId,
    device_id: DeviceId,
    manager: WorkerManager,
}

impl WebWorld {
    /// Boots a live daemon world whose session carries the EXEC override —
    /// the auto-mode shape under which network fetches auto-allow.
    async fn boot(prefix: &str, provider: Arc<FakeProvider>) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                provider_factory: Arc::new(FixedProviderFactory { provider }),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search: None,
            },
            false,
        );
        hub.install_worker_manager(manager.handle())
            .expect("install manager");
        let session_id = SessionId::new(format!("{prefix}-session"));
        let device_id = DeviceId::new(format!("{prefix}-device"));
        let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
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
            permission_overrides: Some(SessionPermissionOverridesV1 {
                allow_writes: false,
                allow_exec: true,
            }),
            effort: None,
            fast: false,
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
        timeout(Duration::from_secs(10), async {
            loop {
                let events = self
                    .store
                    .read(&self.session_id, 0, 2048)
                    .await
                    .expect("read journal");
                if events.iter().any(|event| {
                    event.run_id.as_ref() == Some(&run_id)
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
        .expect("run reaches Done");
        run_id
    }

    async fn typed_payloads(&self) -> Vec<EventPayload> {
        self.store
            .read(&self.session_id, 0, 2048)
            .await
            .expect("read journal")
            .into_iter()
            .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
            .collect()
    }
}

/// One-route loopback HTTP server; every connection gets the same response.
async fn spawn_loopback_server(body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds loopback listener");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let response = response.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

/// LAW (LW6+LW7 daemon halves): a live `web_fetch` call fetches a loopback
/// page through the guarded engine, auto-allows under the exec override,
/// journals the four effect phases with the URL, and lands the fetched text
/// in the tool result. A metadata-address fetch in the SAME session becomes
/// a typed FAILED tool result — the effect journals `Failed` with the URL
/// and the turn still completes.
#[tokio::test]
async fn live_web_fetch_is_brokered_journaled_and_refusals_stay_typed_results() {
    let base = spawn_loopback_server("hello from loopback").await;
    let good_url = format!("{base}/doc");
    let evil_url = "http://169.254.169.254/latest/meta-data/";
    let script = vec![
        FakeStep::EmitToolCall {
            call_id: "fetch-ok".into(),
            name: "web_fetch".into(),
            args: serde_json::json!({ "url": good_url }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "fetch-ok".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "fetch-evil".into(),
            name: "web_fetch".into(),
            args: serde_json::json!({ "url": evil_url }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "fetch-evil".into(),
        },
        FakeStep::EmitText {
            text: "done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let world = WebWorld::boot("wb-fetch", Arc::new(FakeProvider::new(script))).await;
    world.run_turn("wb-fetch", "fetch both pages").await;
    let payloads = world.typed_payloads().await;

    // The fetched text reached the model as the tool result.
    let ok_result = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "fetch-ok" => Some(result),
            _ => None,
        })
        .expect("loopback fetch result");
    assert!(
        ok_result.preview.contains("hello from loopback"),
        "fetched text lands in the result: {}",
        ok_result.preview
    );
    assert!(!ok_result.truncated);

    // The hostile fetch is a TYPED failed result, never a turn failure.
    let evil_result = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "fetch-evil" => Some(result),
            _ => None,
        })
        .expect("hostile fetch still produces a typed result");
    assert!(
        evil_result.preview.starts_with("web_fetch failed:"),
        "typed refusal preview: {}",
        evil_result.preview
    );

    // Effect journal (LW7): two Network intents carrying the URLs, both
    // authorized WITHOUT a menu (exec override = auto-mode), each with an
    // honest terminal outcome.
    let phases: Vec<&EffectPhase> = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Effect(phase) => Some(phase),
            _ => None,
        })
        .collect();
    let intents: Vec<_> = phases
        .iter()
        .filter_map(|phase| match phase {
            EffectPhase::Intent(intent) => Some(intent),
            _ => None,
        })
        .collect();
    assert_eq!(intents.len(), 2, "one intent per fetch: {intents:?}");
    assert!(intents[0].summary.contains(&good_url));
    assert!(intents[1].summary.contains(evil_url));
    assert!(matches!(
        &intents[0].class,
        EffectClass::Network { host } if host == "127.0.0.1"
    ));
    let outcomes: Vec<_> = phases
        .iter()
        .filter_map(|phase| match phase {
            EffectPhase::Outcome {
                effect, outcome, ..
            } => Some((effect, outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2, "each fetch terminalizes");
    assert!(matches!(outcomes[0].1, EffectOutcome::Ok));
    assert!(matches!(
        outcomes[1].1,
        EffectOutcome::Failed { error } if error.contains(evil_url)
    ));
}

/// LAW (LW8 half, advertisement): the local `web_fetch` client tool joins
/// every pack EXCEPT the first-party Anthropic pairs, whose fetch is the
/// SERVER tool of the same name; kimi/OSS/enterprise/openai pairs all carry
/// it, and the child filter still removes exactly `todo_write`.
#[test]
fn web_fetch_advertises_on_every_pair_except_first_party_anthropic() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    for provider in [
        "openai",
        "openai-oauth",
        "openai-compatible",
        "kimi-oauth",
        "gemini",
        "bedrock",
        "vertex",
        "fake",
    ] {
        let pack =
            advertised_tool_definitions(&factory, false, provider, WebCapabilityDegrade::default());
        assert!(
            pack.iter().any(|tool| tool.name == "web_fetch"),
            "`{provider}` advertises the local web_fetch"
        );
    }
    for provider in ["anthropic", "anthropic-oauth"] {
        let pack =
            advertised_tool_definitions(&factory, false, provider, WebCapabilityDegrade::default());
        assert!(
            !pack.iter().any(|tool| tool.name == "web_fetch"),
            "`{provider}` withholds the local tool — the server tool owns the name"
        );
        let child =
            advertised_tool_definitions(&factory, true, provider, WebCapabilityDegrade::default());
        assert!(
            !child.iter().any(|tool| tool.name == "todo_write"),
            "the child filter still applies beside the pair filter"
        );
    }
}
