//! W-B daemon-boundary web laws: the LOCAL `web_fetch` tool end to end
//! through a live daemon run (broker journal + typed refusal results, LW6/
//! LW7 daemon halves), the lite-only CLIENT `web_search` tool and its
//! session degrade (LW4 client half), and the per-pair advertisement seam
//! including a live mid-session pair switch (LW8). Loopback mock servers and
//! injected stubs only — nothing here dials the real network.

#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, TurnToolFactory,
    WebCapabilityDegrade, WebSearchExecutor, WebSearchFailure, WorkerDependencies, WorkerManager,
    advertised_tool_definitions,
};
use haider_core::{
    ProviderAttemptDecision, ProviderAttemptResolver, SessionCreateCommand,
    SessionSelectModelCommand, SessionSelectModelOutcome, SqliteStoreHandle, StoreHandle,
    TurnAcceptCommand, TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::error::{ErrorAction, ErrorPresentation, ErrorScope, HaiderError};
use haider_protocol::ids::{CredentialAlias, DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::FinishReason;
use haider_protocol::session::{SessionMetadataV1, SessionPermissionOverridesV1};
use haider_protocol::state::RunState;
use haider_provider::{
    ANTHROPIC_OAUTH_PROVIDER_NAME, FakeProvider, FakeStep, OPENAI_OAUTH_PROVIDER_NAME, Provider,
};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};

/// Routes each turn to the fake registered under `metadata.provider`, so a
/// mid-session pair switch is an OBSERVED landing rather than an inference.
/// `provider_name` mirrors the session metadata (the R6 contract), which is
/// exactly what the advertisement seam derives from.
struct FixedProviderFactory {
    providers: HashMap<String, Arc<FakeProvider>>,
    fallback_resolver: Option<Arc<dyn ProviderAttemptResolver>>,
}

impl FixedProviderFactory {
    fn single(pair: &str, provider: Arc<FakeProvider>) -> Self {
        Self {
            providers: HashMap::from([(pair.to_owned(), provider)]),
            fallback_resolver: None,
        }
    }

    fn single_with_web_fallback(pair: &str, provider: Arc<FakeProvider>) -> Self {
        Self {
            providers: HashMap::from([(pair.to_owned(), Arc::clone(&provider))]),
            fallback_resolver: Some(Arc::new(TestWebFallbackResolver { provider })),
        }
    }
}

#[derive(Debug)]
struct TestWebFallbackResolver {
    provider: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderAttemptResolver for TestWebFallbackResolver {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        error: &haider_provider::ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        Ok(
            if error.presentation.subcode.as_str() == "provider-web-tool-rejected" {
                ProviderAttemptDecision::Fallback {
                    provider: Arc::clone(&self.provider) as Arc<dyn Provider>,
                    account: current_account.clone(),
                }
            } else {
                ProviderAttemptDecision::Stop
            },
        )
    }
}

#[async_trait::async_trait]
impl ProviderFactory for FixedProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let provider = self.providers.get(&metadata.provider).ok_or_else(|| {
            HaiderError::new(
                haider_protocol::error::ErrorCode::ProviderError,
                format!("no injected fake for provider {}", metadata.provider),
                false,
            )
        })?;
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(provider) as Arc<dyn Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: self
                .fallback_resolver
                .as_ref()
                .map(|_| "test-web-account".to_owned()),
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: self.fallback_resolver.clone(),
            compaction_promotion: None,
        })
    }
}

/// Scripted stand-in for the subscription search executor: records every
/// call and answers from a queue, so the alpha/search endpoint is never
/// dialed and a 404/410 degrade is expressible as a fixture.
#[derive(Default)]
struct StubWebSearch {
    calls: Mutex<Vec<(String, String, String)>>,
    answers: Mutex<VecDeque<Result<String, WebSearchFailure>>>,
}

impl StubWebSearch {
    fn with_answers(answers: Vec<Result<String, WebSearchFailure>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answers: Mutex::new(answers.into()),
        })
    }

    fn calls(&self) -> Vec<(String, String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl WebSearchExecutor for StubWebSearch {
    async fn search(
        &self,
        model: &str,
        session_id: &str,
        query: &str,
    ) -> Result<String, WebSearchFailure> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((model.to_owned(), session_id.to_owned(), query.to_owned()));
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Ok(String::from("no scripted answer")))
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
    /// Boots a live daemon world on the `fake` pair whose session carries the
    /// EXEC override — the auto-mode shape under which network fetches
    /// auto-allow.
    async fn boot(prefix: &str, provider: Arc<FakeProvider>) -> Self {
        Self::boot_with(
            prefix,
            "fake",
            "fake-model",
            FixedProviderFactory::single("fake", provider),
            None,
        )
        .await
    }

    /// The general form: an explicit starting pair, an explicit routing
    /// factory, and an optional injected `web_search` executor.
    async fn boot_with(
        prefix: &str,
        pair: &str,
        model: &str,
        factory: FixedProviderFactory,
        web_search: Option<Arc<dyn WebSearchExecutor>>,
    ) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        hub.install_creatable_providers(
            factory
                .providers
                .keys()
                .cloned()
                .collect::<BTreeSet<String>>(),
        )
        .expect("install creatable providers");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(factory),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
                web_search,
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
            provider: pair.into(),
            model: model.into(),
            max_tokens: 4096,
            permission_overrides: Some(SessionPermissionOverridesV1 {
                allow_writes: false,
                allow_exec: true,
                allow_mobile: false,
                auto_allow: false,
            }),
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
        }
    }

    /// Commits a mid-session pair switch through the durable selection path
    /// (the same command the RPC surface issues).
    async fn switch_pair(&self, command_id: &str, provider: &str, model: &str) {
        let worker_generation = self.store.worker_generation();
        let request_json = serde_json::json!({
            "session_id": self.session_id,
            "worker_generation": worker_generation,
            "model": model,
            "provider": provider,
        })
        .to_string();
        let outcome = self
            .store
            .select_session_model(SessionSelectModelCommand {
                command_id: command_id.to_owned(),
                request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
                request_json,
                session_id: self.session_id.clone(),
                worker_generation,
                provider: provider.to_owned(),
                model: model.to_owned(),
                expected_pair: None,
                event_id: EventId::new(format!("{command_id}-event")),
                device_id: self.device_id.clone(),
            })
            .await
            .expect("select model");
        assert!(
            matches!(outcome, SessionSelectModelOutcome::Committed { .. }),
            "the switch must commit before the next turn"
        );
    }

    async fn run_turn(&self, label: &str, text: &str) -> RunId {
        self.run_turn_until(label, text, RunState::Done).await
    }

    /// Runs one turn and waits for an EXPECTED terminal — `Done` for the
    /// happy paths, `Failed` for the provider-refusal law.
    async fn run_turn_until(&self, label: &str, text: &str, terminal: RunState) -> RunId {
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
                            |payload| {
                                matches!(payload, EventPayload::RunState(state) if state == terminal)
                            },
                        )
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("run reaches {terminal:?}"));
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
            advertised_tool_definitions(&factory, None, provider, WebCapabilityDegrade::default());
        assert!(
            pack.iter().any(|tool| tool.name == "web_fetch"),
            "`{provider}` advertises the local web_fetch"
        );
    }
    for provider in ["anthropic", "anthropic-oauth"] {
        let pack =
            advertised_tool_definitions(&factory, None, provider, WebCapabilityDegrade::default());
        assert!(
            !pack.iter().any(|tool| tool.name == "web_fetch"),
            "`{provider}` withholds the local tool — the server tool owns the name"
        );
        let child = advertised_tool_definitions(
            &factory,
            Some(&crate::worker::default_child_grant()),
            provider,
            WebCapabilityDegrade::default(),
        );
        assert!(
            !child.iter().any(|tool| tool.name == "todo_write"),
            "the child filter still applies beside the pair filter"
        );
        // Decision 1's "local fallback on refusal": once this session's
        // SERVER web tools 400ed, the local tool returns to the pack.
        let fallback = advertised_tool_definitions(
            &factory,
            None,
            provider,
            WebCapabilityDegrade {
                anthropic_web_tools: true,
                openai_alpha_search: false,
                disable_hosted_web_tools: false,
            },
        );
        assert!(
            fallback.iter().any(|tool| tool.name == "web_fetch"),
            "`{provider}` falls back to the local tool once the server tools degrade"
        );
    }
}

/// LAW (LW4 client half, advertisement): the CLIENT `web_search` function
/// tool exists for responses-lite pairs ONLY — every other family either has
/// a provider-native search or honestly none — children inherit the same
/// derivation, and a 404/410 from the unofficial endpoint takes it out of
/// the pack for the rest of the session.
#[test]
fn client_web_search_advertises_on_lite_only_and_a_gone_endpoint_unadvertises_it() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let lite = advertised_tool_definitions(
        &factory,
        None,
        OPENAI_OAUTH_PROVIDER_NAME,
        WebCapabilityDegrade::default(),
    );
    assert!(
        lite.iter().any(|tool| tool.name == "web_search"),
        "the responses-lite pair carries the client search"
    );
    // Decision 8: subagents inherit the same derivation.
    let child = advertised_tool_definitions(
        &factory,
        Some(&crate::worker::default_child_grant()),
        OPENAI_OAUTH_PROVIDER_NAME,
        WebCapabilityDegrade::default(),
    );
    assert!(
        child.iter().any(|tool| tool.name == "web_search"),
        "a delegated child on the lite pair keeps the client search"
    );
    for provider in [
        "openai",
        "anthropic",
        "anthropic-oauth",
        "openai-compatible",
        "kimi-oauth",
        "gemini",
        "bedrock",
        "vertex",
        "fake",
    ] {
        let pack =
            advertised_tool_definitions(&factory, None, provider, WebCapabilityDegrade::default());
        assert!(
            !pack.iter().any(|tool| tool.name == "web_search"),
            "`{provider}` must NOT carry the lite-only client search"
        );
    }
    let degraded = advertised_tool_definitions(
        &factory,
        None,
        OPENAI_OAUTH_PROVIDER_NAME,
        WebCapabilityDegrade {
            anthropic_web_tools: false,
            openai_alpha_search: true,
            disable_hosted_web_tools: false,
        },
    );
    assert!(
        !degraded.iter().any(|tool| tool.name == "web_search"),
        "a gone alpha/search endpoint stops the advertisement (no retry storm)"
    );
    // The degrade is capability-scoped: the local fetch is untouched.
    assert!(degraded.iter().any(|tool| tool.name == "web_fetch"));
}

/// LAW (LW4 client half, execution): on a responses-lite pair the client
/// `web_search` call reaches the subscription executor with THIS turn's
/// model and session id, and its answer lands in the tool result bounded by
/// the 32 KiB cap with an honest truncation marker.
#[tokio::test]
async fn live_web_search_executes_on_lite_with_the_turn_identity_and_bounds_its_text() {
    let long_answer = "s".repeat(40 * 1024);
    let executor = StubWebSearch::with_answers(vec![Ok(long_answer)]);
    let script = vec![
        FakeStep::EmitToolCall {
            call_id: "search-one".into(),
            name: "web_search".into(),
            args: serde_json::json!({ "query": "rust sse decoding" }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "search-one".into(),
        },
        FakeStep::EmitText {
            text: "done".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let fake = Arc::new(FakeProvider::new(script));
    let world = WebWorld::boot_with(
        "wb-search",
        OPENAI_OAUTH_PROVIDER_NAME,
        "gpt-5.6-sol",
        FixedProviderFactory::single(OPENAI_OAUTH_PROVIDER_NAME, Arc::clone(&fake)),
        Some(Arc::clone(&executor) as Arc<dyn WebSearchExecutor>),
    )
    .await;
    world.run_turn("wb-search", "search the web").await;

    // The tool WAS advertised to the model on this pair.
    let requests = fake.requests();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "web_search"),
        "the lite pair advertises the client search"
    );

    // Exactly one execution, carrying the turn's identity.
    assert_eq!(
        executor.calls(),
        vec![(
            "gpt-5.6-sol".to_owned(),
            "wb-search-session".to_owned(),
            "rust sse decoding".to_owned(),
        )]
    );

    let result = world
        .typed_payloads()
        .await
        .into_iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "search-one" => Some(result),
            _ => None,
        })
        .expect("search result");
    assert!(result.truncated, "40 KiB of answer exceeds the 32 KiB cap");
    assert!(
        result.preview.ends_with("[web_search: output truncated]"),
        "the cap is honest: {}",
        &result.preview[result.preview.len().saturating_sub(80)..]
    );
    assert!(result.preview.len() <= 32 * 1024 + 64);
}

/// LAW (LW4 client half, degrade): a 404/410 from the unofficial
/// alpha/search endpoint is a TYPED tool result, not a turn failure, and it
/// latches the session capability off — the NEXT turn's advertised pack no
/// longer contains `web_search`, so the model cannot start a retry storm.
#[tokio::test]
async fn a_gone_alpha_search_endpoint_degrades_the_session_for_the_next_turn() {
    let executor = StubWebSearch::with_answers(vec![Err(WebSearchFailure {
        message: "the subscription search endpoint answered HTTP 404".into(),
        degraded: true,
    })]);
    let script = vec![
        FakeStep::EmitToolCall {
            call_id: "search-gone".into(),
            name: "web_search".into(),
            args: serde_json::json!({ "query": "anything" }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "search-gone".into(),
        },
        FakeStep::EmitText {
            text: "gave up".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "second turn".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let fake = Arc::new(FakeProvider::new(script));
    let world = WebWorld::boot_with(
        "wb-gone",
        OPENAI_OAUTH_PROVIDER_NAME,
        "gpt-5.6-sol",
        FixedProviderFactory::single(OPENAI_OAUTH_PROVIDER_NAME, Arc::clone(&fake)),
        Some(Arc::clone(&executor) as Arc<dyn WebSearchExecutor>),
    )
    .await;
    world.run_turn("wb-gone-one", "search the web").await;

    let result = world
        .typed_payloads()
        .await
        .into_iter()
        .find_map(|payload| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "search-gone" => {
                Some(result)
            }
            _ => None,
        })
        .expect("gone-endpoint result");
    assert!(
        result.preview.starts_with("web_search failed:"),
        "a dead endpoint is a typed result: {}",
        result.preview
    );
    assert!(result.preview.contains("404"), "the reason surfaces");

    world.run_turn("wb-gone-two", "try again").await;
    let requests = fake.requests();
    assert_eq!(requests.len(), 3, "two turns, the first with a tool round");
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "web_search"),
        "turn 1 still advertised the capability"
    );
    assert!(
        !requests[2]
            .tools
            .iter()
            .any(|tool| tool.name == "web_search"),
        "turn 2 must not re-offer the gone capability"
    );
    assert_eq!(
        executor.calls().len(),
        1,
        "the endpoint is probed once per session, never in a storm"
    );
}

/// LAW (LW8): the per-turn tool advertisement derives from the RESOLVED
/// pair, so a mid-session switch reshapes it on the NEXT turn — in BOTH
/// directions at once. Leaving responses-lite drops the client `web_search`;
/// arriving on a first-party Anthropic pair also drops the local
/// `web_fetch`, whose name the SERVER tool owns there.
#[tokio::test]
async fn pair_switch_reshapes_the_web_tool_advertisement_on_the_next_turn() {
    let lite = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "from lite".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let anthropic = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "from anthropic".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = WebWorld::boot_with(
        "wb-switch",
        OPENAI_OAUTH_PROVIDER_NAME,
        "gpt-5.6-sol",
        FixedProviderFactory {
            providers: HashMap::from([
                (OPENAI_OAUTH_PROVIDER_NAME.to_owned(), Arc::clone(&lite)),
                (
                    ANTHROPIC_OAUTH_PROVIDER_NAME.to_owned(),
                    Arc::clone(&anthropic),
                ),
            ]),
            fallback_resolver: None,
        },
        Some(StubWebSearch::with_answers(Vec::new()) as Arc<dyn WebSearchExecutor>),
    )
    .await;

    world.run_turn("wb-switch-one", "first").await;
    let lite_tools = &lite.requests()[0].tools;
    assert!(
        lite_tools.iter().any(|tool| tool.name == "web_search"),
        "the lite pair carries the client search"
    );
    assert!(
        lite_tools.iter().any(|tool| tool.name == "web_fetch"),
        "…and the universal local fetch"
    );

    world
        .switch_pair(
            "wb-switch-select",
            ANTHROPIC_OAUTH_PROVIDER_NAME,
            "claude-web",
        )
        .await;

    world.run_turn("wb-switch-two", "second").await;
    let anthropic_tools = &anthropic.requests()[0].tools;
    assert!(
        !anthropic_tools.iter().any(|tool| tool.name == "web_search"),
        "the client search does not follow the session off responses-lite"
    );
    assert!(
        !anthropic_tools.iter().any(|tool| tool.name == "web_fetch"),
        "the anthropic pair withholds the local fetch — the SERVER tool owns the name"
    );
    assert_eq!(lite.requests().len(), 1, "turn 2 did not land on lite");
}

/// LAW (W-B decision 1, "local fallback on refusal" — daemon half): an org
/// can disable the Anthropic server web tools, and the DECLARED tool then
/// 400s. The exact typed refusal triggers one labeled retry in the SAME turn
/// with local `web_fetch`. A generic invalid request must never spend that
/// capability fallback.
#[tokio::test]
async fn an_invalid_request_on_an_anthropic_turn_falls_back_to_the_local_fetch() {
    let script = vec![
        // Turn 1: a NON-refusal failure — the capability survives it.
        // (Not a retryable kind: this law is about the discriminator, not
        // about the retry ladder.)
        FakeStep::Error {
            kind: haider_provider::ProviderErrorKind::ContextExceeded,
            message: "prompt is too long".into(),
            retry_after_ms: None,
        },
        // Turn 2: the org-disabled shape.
        FakeStep::ErrorPresented {
            kind: haider_provider::ProviderErrorKind::InvalidRequest,
            message: "tool `web_search_20250305` is not available for this organization".into(),
            presentation: ErrorPresentation::new(
                "provider-web-tool-rejected",
                "Provider web tool unavailable",
                "use the local equivalent",
                ErrorScope::Tool,
                [ErrorAction::Retry],
            ),
        },
        // Turn 2's bounded same-turn fallback.
        FakeStep::EmitText {
            text: "fetched locally".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ];
    let fake = Arc::new(FakeProvider::new(script));
    let world = WebWorld::boot_with(
        "wb-degrade",
        ANTHROPIC_OAUTH_PROVIDER_NAME,
        "claude-web",
        FixedProviderFactory::single_with_web_fallback(
            ANTHROPIC_OAUTH_PROVIDER_NAME,
            Arc::clone(&fake),
        ),
        None,
    )
    .await;

    world
        .run_turn_until("wb-degrade-one", "first", RunState::Errored)
        .await;
    world.run_turn("wb-degrade-two", "second").await;

    let requests = fake.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "web_fetch"),
        "the server tool owns the name while the capability is healthy"
    );
    assert!(
        !requests[1]
            .tools
            .iter()
            .any(|tool| tool.name == "web_fetch"),
        "a non-refusal failure must not spend the capability"
    );
    assert!(
        requests[2]
            .tools
            .iter()
            .any(|tool| tool.name == "web_fetch"),
        "the bounded same-turn retry uses the LOCAL fetch tool"
    );
    assert!(
        requests[1].messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    haider_protocol::provider::Block::Text { text } if text == "first"
                )
            })
        }),
        "the errored turn's committed user message stays in the next prompt"
    );
    let history = world
        .store
        .read(&world.session_id, 0, 256)
        .await
        .expect("fallback history");
    assert!(history.iter().any(|event| {
        matches!(
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::Item(haider_protocol::item::ItemEvent::Completed {
                item: haider_protocol::item::TurnItem::Extension { kind, data, .. },
                ..
            })) if kind == "provider_tool_fallback"
                && data.get("label").and_then(serde_json::Value::as_str)
                    == Some("provider hosted web tool rejected — using local web_fetch")
        )
    }));
}
