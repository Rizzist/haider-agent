#![allow(clippy::expect_used)]

//! Runtime laws for provider-agnostic model selection (F1): the committed
//! pair is what the NEXT logical turn — and every child spawned from it —
//! resolves through. Two recording fake providers make the landing provider
//! an asserted fact, not an inference.

use crate::accounts::ConnectionTransport;
use crate::session_hub::{
    FrameSendError, FrameSink, SessionHub, SessionHubConfig, SessionHubError,
};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use haider_core::{
    ProviderAttemptDecision, ProviderAttemptResolver, ProviderPairSwitchCause,
    ProviderPairSwitchTarget, QueueConsumeCommand, QueuePromoteCommand, SessionCreateCommand,
    SessionSelectEffortCommand, SessionSelectEffortOutcome, SessionSelectFastCommand,
    SessionSelectFastOutcome, SessionSelectModelCommand, SessionSelectModelOutcome,
    SqliteStoreHandle, StoreHandle, TurnAcceptCommand, TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::cache::{CacheEpochTransitionReason, CacheEpochTransitionV1};
use haider_protocol::envelope::{EventEnvelope, PromptRender, RenderTargets, SCHEMA_VERSION};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{CredentialAlias, DeviceId, EventId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::session::{
    EffortSelected, FastModeSelected, ModelSelected, SessionMetadataV1,
};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep, ProviderError};
use haider_rpc::{
    AttachMode, Capability, CommandId, ModelInventoryAuthorityWire, ProviderApiFamilyWire,
    ProviderAvailabilityWire, ProviderSummaryWire, ProviderTrustWire, RequestBody, RequestId,
    ResponseBody, WireFrame,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, timeout};

#[derive(Default)]
struct GraphSelectionSink(Mutex<Vec<WireFrame>>);

impl FrameSink for GraphSelectionSink {
    fn try_send(&self, frame: WireFrame) -> Result<(), FrameSendError> {
        self.0.lock().expect("graph sink lock").push(frame);
        Ok(())
    }
}

fn graph_response(sink: &GraphSelectionSink, request_id: &str) -> Option<haider_rpc::ResponseBody> {
    sink.0
        .lock()
        .expect("graph sink frames")
        .iter()
        .find_map(|frame| match frame {
            WireFrame::Response {
                request_id: found,
                body,
            } if found.as_str() == request_id => Some(body.clone()),
            _ => None,
        })
}

/// Routes each turn to the fake registered for `metadata.provider` — the
/// injected analog of per-provider adapters, echoing the R6 contract
/// (`provider_name` mirrors the session metadata).
struct RoutingProviderFactory {
    providers: HashMap<String, Arc<FakeProvider>>,
    cache_reconciliations: Arc<Mutex<Vec<(SessionId, String)>>>,
    fallback_enabled: bool,
}

#[derive(Debug)]
struct RuntimeFallbackResolver {
    target: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderAttemptResolver for RuntimeFallbackResolver {
    async fn resolve(
        &self,
        _current_account: &CredentialAlias,
        _error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        // Pins the rotation-exhausted entry point: current-provider account
        // resolution has no alternate and core must consult fallback next.
        Ok(ProviderAttemptDecision::Stop)
    }

    async fn resolve_fallback(
        &self,
        _current_account: &CredentialAlias,
        _error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        Ok(ProviderAttemptDecision::Switch(ProviderPairSwitchTarget {
            provider: Arc::clone(&self.target) as Arc<dyn haider_provider::Provider>,
            account: CredentialAlias::new("fake-b-account"),
            provider_name: "fake-b".into(),
            model: "model-b".into(),
            context_window: None,
            cached_input_is_subset: true,
            provider_request_state: Default::default(),
            auth_scope: "api_key".into(),
            attempt_resolver: None,
            cause: ProviderPairSwitchCause::FallbackChain,
        }))
    }
}

#[async_trait::async_trait]
impl ProviderFactory for RoutingProviderFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        let provider = self.providers.get(&metadata.provider).ok_or_else(|| {
            HaiderError::new(
                ErrorCode::ProviderError,
                format!("no injected fake for provider {}", metadata.provider),
                false,
            )
        })?;
        let attempt_resolver =
            (self.fallback_enabled && metadata.provider == "fake-a").then(|| {
                Arc::new(RuntimeFallbackResolver {
                    target: Arc::clone(
                        self.providers
                            .get("fake-b")
                            .expect("fallback target is registered"),
                    ),
                }) as Arc<dyn ProviderAttemptResolver>
            });
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(provider) as Arc<dyn haider_provider::Provider>,
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: attempt_resolver
                .as_ref()
                .map(|_| "fake-a-account".to_owned()),
            active_no_auth: false,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver,
            compaction_promotion: None,
        })
    }

    async fn reconcile_cache_scope(&self, session_id: &SessionId, provider: &str) {
        self.cache_reconciliations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((session_id.clone(), provider.to_owned()));
    }
}

struct PairSwitchWorld {
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: WorkerManager,
    session_id: SessionId,
    device_id: DeviceId,
    cache_reconciliations: Arc<Mutex<Vec<(SessionId, String)>>>,
}

impl PairSwitchWorld {
    /// Store + hub + manager with fakes `fake-a`/`fake-b` creatable, and one
    /// session created on the (`fake-a`, `model-a`) pair.
    async fn boot(prefix: &str, fake_a: Arc<FakeProvider>, fake_b: Arc<FakeProvider>) -> Self {
        Self::boot_with_fallback(prefix, fake_a, fake_b, false).await
    }

    async fn boot_with_catalog(
        prefix: &str,
        fake_a: Arc<FakeProvider>,
        fake_b: Arc<FakeProvider>,
        providers: Vec<ProviderSummaryWire>,
    ) -> Self {
        let world = Self::boot(prefix, fake_a, fake_b).await;
        world
            .hub
            .install_accounts(crate::accounts::AccountsFacade {
                login: None,
                oauth: None,
                snapshot: Arc::new(Mutex::new(Vec::new())),
                management: crate::accounts::ManagementSnapshot::new(1, Vec::new(), providers),
                vault_supported: false,
                discovery_disabled: true,
                device_discovery: crate::accounts::DeviceDiscoverySnapshot::new(true),
                vault: None,
                sources: Arc::new(Mutex::new(Vec::new())),
            })
            .expect("install fixture model catalog");
        world
    }

    async fn boot_with_fallback(
        prefix: &str,
        fake_a: Arc<FakeProvider>,
        fake_b: Arc<FakeProvider>,
        fallback_enabled: bool,
    ) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        // Leak the tempdir handle so the profile outlives this constructor;
        // the OS reclaims the tree, matching sibling runtime tests' lifetime.
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        hub.install_creatable_providers(BTreeSet::from(["fake-a".to_owned(), "fake-b".to_owned()]))
            .expect("install creatable providers");
        let cache_reconciliations = Arc::new(Mutex::new(Vec::new()));
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(RoutingProviderFactory {
                    providers: HashMap::from([
                        ("fake-a".to_owned(), fake_a),
                        ("fake-b".to_owned(), fake_b),
                    ]),
                    cache_reconciliations: Arc::clone(&cache_reconciliations),
                    fallback_enabled,
                }),
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
            provider: "fake-a".into(),
            model: "model-a".into(),
            max_tokens: 4096,
            permission_overrides: None,
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
            manager,
            session_id,
            device_id,
            cache_reconciliations,
        }
    }

    async fn run_turn(&self, label: &str, text: &str) -> RunId {
        let (run_id, disposition) = self.submit_turn(label, text, DeliveryMode::Steer).await;
        assert_eq!(disposition, TurnAdmissionDisposition::Started);
        self.await_done(&run_id).await;
        run_id
    }

    /// Accepts + hands the turn to the manager WITHOUT asserting the
    /// disposition or awaiting the terminal — the compaction-window laws
    /// need to observe `Queued` admissions that only run later.
    async fn submit_turn(
        &self,
        label: &str,
        text: &str,
        mode: DeliveryMode,
    ) -> (RunId, TurnAdmissionDisposition) {
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
                mode,
                queued_event_id: EventId::new(format!("{label}-queued")),
                user_event_id: EventId::new(format!("{label}-user")),
                active_event_id: EventId::new(format!("{label}-active")),
                device_id: self.device_id.clone(),
            })
            .await
            .expect("accept turn");
        let disposition = accepted.disposition;
        self.manager
            .handle()
            .submit(accepted)
            .await
            .expect("submit turn");
        (run_id, disposition)
    }

    /// Kicks off manual compaction on a background task (bounded Busy retry
    /// through the post-Done settle window), then waits until the journal
    /// actually says `Compacting` so callers act INSIDE the window.
    async fn start_compaction_and_await_window(
        &self,
        command_id: &str,
    ) -> tokio::task::JoinHandle<()> {
        let manager = self.manager.handle();
        let session_id = self.session_id.clone();
        let generation = self.store.worker_generation();
        let command = command_id.to_owned();
        let handle = tokio::spawn(async move {
            let mut attempt = 0;
            loop {
                match manager
                    .compact(session_id.clone(), command.clone(), generation, None)
                    .await
                {
                    Ok(_) => break,
                    Err(error) if error.code == ErrorCode::Busy && attempt < 40 => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(error) => panic!("manual compaction failed: {error:?}"),
                }
            }
        });
        timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .latest_run_states()
                    .await
                    .iter()
                    .any(|(_, state)| matches!(state, RunState::Compacting))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("compaction window opens (journal says Compacting)");
        handle
    }

    /// Latest committed `RunState` per run, in journal order.
    async fn latest_run_states(&self) -> Vec<(RunId, RunState)> {
        let mut latest: Vec<(RunId, RunState)> = Vec::new();
        for event in self
            .store
            .read(&self.session_id, 0, 2048)
            .await
            .expect("read journal")
        {
            let (Some(run_id), Ok(EventPayload::RunState(state))) =
                (event.run_id.clone(), event.payload.decode_event())
            else {
                continue;
            };
            if let Some(slot) = latest.iter_mut().find(|(id, _)| id == &run_id) {
                slot.1 = state;
            } else {
                latest.push((run_id, state));
            }
        }
        latest
    }

    async fn await_done(&self, run_id: &RunId) {
        timeout(Duration::from_secs(10), async {
            loop {
                let events = self
                    .store
                    .read(&self.session_id, 0, 1024)
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run reaches Done");
    }

    async fn await_run_state(&self, run_id: &RunId, expected: RunState) {
        timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .latest_run_states()
                    .await
                    .iter()
                    .any(|(candidate, state)| candidate == run_id && state == &expected)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run reaches expected state");
    }

    /// The RESOLVED (`fake-b`, `model-b`) selection command with a stable
    /// receipt identity.
    fn select_command(&self, command_id: &str) -> SessionSelectModelCommand {
        self.select_command_at_generation(command_id, self.store.worker_generation())
    }

    /// The same selection command pinned to an explicit worker generation —
    /// the request JSON and digest carry the same generation the command
    /// claims, so identity validation observes the fence, not a digest
    /// mismatch.
    fn select_command_at_generation(
        &self,
        command_id: &str,
        worker_generation: u64,
    ) -> SessionSelectModelCommand {
        let request_json = serde_json::json!({
            "session_id": self.session_id,
            "worker_generation": worker_generation,
            "model": "model-b",
            "provider": "fake-b",
        })
        .to_string();
        SessionSelectModelCommand {
            command_id: command_id.to_owned(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: self.session_id.clone(),
            worker_generation,
            provider: "fake-b".into(),
            model: "model-b".into(),
            expected_pair: None,
            event_id: EventId::new(format!("{command_id}-event")),
            device_id: self.device_id.clone(),
        }
    }

    /// A RESOLVED effort selection command with a stable receipt identity
    /// (G3 — the select_command twin).
    fn effort_command(&self, command_id: &str, effort: Option<&str>) -> SessionSelectEffortCommand {
        let effort = effort.map(str::to_owned);
        let worker_generation = self.store.worker_generation();
        let request_json = serde_json::json!({
            "session_id": self.session_id,
            "worker_generation": worker_generation,
            "effort": effort,
        })
        .to_string();
        SessionSelectEffortCommand {
            command_id: command_id.to_owned(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: self.session_id.clone(),
            worker_generation,
            effort,
            event_id: EventId::new(format!("{command_id}-event")),
            device_id: self.device_id.clone(),
        }
    }

    /// A VALIDATED fast-mode toggle command with a stable receipt identity.
    fn fast_command(&self, command_id: &str, enabled: bool) -> SessionSelectFastCommand {
        let worker_generation = self.store.worker_generation();
        let request_json = serde_json::json!({
            "session_id": self.session_id,
            "worker_generation": worker_generation,
            "enabled": enabled,
        })
        .to_string();
        SessionSelectFastCommand {
            command_id: command_id.to_owned(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: self.session_id.clone(),
            worker_generation,
            enabled,
            event_id: EventId::new(format!("{command_id}-event")),
            device_id: self.device_id.clone(),
        }
    }

    async fn shutdown(self) {
        self.manager.shutdown().await.expect("manager shutdown");
        self.hub.shutdown().await.expect("hub shutdown");
        self.store.close().await.expect("store close");
    }
}

fn text_turn(text: &str) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitText { text: text.into() },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]
}

fn model_summary(provider: &str, models: &[&str]) -> ProviderSummaryWire {
    ProviderSummaryWire {
        provider: provider.to_owned(),
        api_family: ProviderApiFamilyWire::OpenAiResponses,
        endpoint: None,
        response_open_timeout_ms: None,
        chunk_idle_timeout_ms: None,
        semantic_progress_timeout_ms: None,
        models: models.iter().map(|model| (*model).to_owned()).collect(),
        model_details: Vec::new(),
        inventory_fetched_at_ms: None,
        inventory_authority: ModelInventoryAuthorityWire::Authoritative,
        auth_methods: Vec::new(),
        availability: ProviderAvailabilityWire::Available,
        availability_reason: None,
        default_model: models.first().map(|model| (*model).to_owned()),
        enabled: true,
        trust: ProviderTrustWire::Full,
    }
}

/// ITEM #3 strand regression: `start_turn` must resolve the immutable Loom
/// revision named by GraphPinned, not the registry's newer current row.
#[tokio::test]
async fn pinned_loom_workflow_runs_its_next_turn_after_registry_edit() {
    let fake_a = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "retained-workflow-green".into(),
            name: "graph_evidence".into(),
            args: serde_json::json!({
                "node": "STEP",
                "verdict": "green",
                "detail": "retained workflow revision completed"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "retained-workflow-green".into(),
        },
        FakeStep::EmitText {
            text: "retained revision ran".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = PairSwitchWorld::boot(
        "retained-workflow-edit",
        Arc::clone(&fake_a),
        Arc::new(FakeProvider::new(Vec::new())),
    )
    .await;
    let first_registration = world
        .store
        .loom_register_workflow("runtime-retained: A -> A\nstep \"one\" :cmd".into())
        .await
        .expect("register rev 1");
    let first = world
        .store
        .loom_workflow("runtime-retained".into())
        .await
        .expect("read rev 1")
        .expect("rev 1 exists");
    let first_digest = haider_protocol::graph::graph_template_digest(&first.template);

    let sink = Arc::new(GraphSelectionSink::default());
    let connection = world
        .hub
        .open_connection(
            BTreeSet::from([Capability::View, Capability::Control]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("graph control connection");
    connection
        .request(
            RequestId::new("retained-workflow-attach"),
            RequestBody::SessionAttach {
                session_id: world.session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach graph control");
    connection
        .request(
            RequestId::new("retained-workflow-pin"),
            RequestBody::GraphPin {
                command_id: CommandId::new("retained-workflow-pin-command"),
                session_id: world.session_id.clone(),
                worker_generation: world.store.worker_generation(),
                template: "runtime-retained".into(),
                expected_digest: Some(first_digest.clone()),
            },
        )
        .await
        .expect("fenced graph pin routes");
    let Some(ResponseBody::GraphPin { digest, .. }) =
        graph_response(&sink, "retained-workflow-pin")
    else {
        panic!("rev 1 graph pin succeeds")
    };
    assert_eq!(digest, first_digest);

    let revised = world
        .store
        .loom_register_workflow_cas(
            "runtime-retained: A -> A\nstep \"two\" :cmd".into(),
            haider_protocol::loom::LoomRevisionExpectation {
                rev: first_registration.rev,
                digest: Some(first_registration.digest),
            },
        )
        .await
        .expect("register rev 2");
    let haider_core::LoomRegistryMutation::Applied { value: revised, .. } = revised else {
        panic!("current workflow expectation cannot conflict");
    };
    assert_eq!(revised.rev, 2);

    world
        .run_turn(
            "retained-workflow-next-turn",
            "continue the pinned workflow",
        )
        .await;
    let requests = fake_a.requests();
    assert_eq!(
        requests.len(),
        2,
        "the retained workflow must complete its evidence tool round-trip"
    );
    let first_request_text = requests[0]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.to_owned_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        first_request_text.contains("loom runtime-retained rev 1")
            && first_request_text.contains("step \"one\"")
            && !first_request_text.contains("step \"two\""),
        "the next turn must execute the exact retained rev 1 bytes: {first_request_text}"
    );

    connection.close().await.expect("connection closes");
    world.shutdown().await;
}

/// LAW: an authentication wall with no within-provider alternate commits the
/// exact receipted pair-selection transaction, surfaces both the cold epoch
/// and human-readable hop reason, and finishes the SAME run on provider B.
///
/// MUTATION CHECK: return the original Stop, store-direct the metadata edit,
/// omit either extension, or defer provider B until the next turn. Expected
/// failure: the run/receipt/fact/request assertions below fail independently.
#[tokio::test]
async fn fallback_chain_switch_is_durable_visible_and_finishes_the_same_turn() {
    let fake_a = Arc::new(FakeProvider::new(vec![FakeStep::Error {
        kind: haider_provider::ProviderErrorKind::Authentication,
        message: "provider A auth wall".into(),
        retry_after_ms: None,
    }]));
    let fake_b = Arc::new(FakeProvider::new(text_turn("same turn answer from b")));
    let world = PairSwitchWorld::boot_with_fallback(
        "fallback-runtime",
        fake_a.clone(),
        fake_b.clone(),
        true,
    )
    .await;

    let run_id = world
        .run_turn("fallback-runtime-turn", "survive provider A")
        .await;
    assert_eq!(fake_a.requests().len(), 1);
    assert_eq!(fake_b.requests().len(), 1, "same run continues on B");
    assert_eq!(fake_b.requests()[0].model, "model-b");

    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(
        (metadata.provider.as_str(), metadata.model.as_str()),
        ("fake-b", "model-b")
    );

    let request_json = serde_json::json!({
        "automatic": true,
        "session_id": world.session_id,
        "run_id": run_id,
        "switch_ordinal": 0,
        "worker_generation": world.store.worker_generation(),
        "from_provider": "fake-a",
        "from_model": "model-a",
        "provider": "fake-b",
        "model": "model-b",
        "cause": "fallback_chain",
    })
    .to_string();
    let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
    let command_id = format!(
        "automatic-pair-switch-{}",
        request_digest.get(..24).expect("digest prefix")
    );
    let receipt = world
        .store
        .session_select_model_receipt(command_id, request_digest, request_json)
        .await
        .expect("receipt lookup")
        .expect("automatic selection receipt");
    assert_eq!(
        (receipt.provider.as_str(), receipt.model.as_str()),
        ("fake-b", "model-b")
    );

    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("journal");
    assert!(events.iter().any(|event| {
        ModelSelected::from_payload_value(&event.payload)
            .is_some_and(|fact| fact.provider == "fake-b" && fact.model == "model-b")
    }));
    let completed_extensions = events.iter().filter_map(|event| {
        let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
            event.payload.decode_event()
        else {
            return None;
        };
        Some(item)
    });
    let extensions = completed_extensions.collect::<Vec<_>>();
    assert!(extensions.iter().any(|item| {
        CacheEpochTransitionV1::from_extension_item(item).is_some_and(|transition| {
            transition.reason == CacheEpochTransitionReason::ConfigurationChanged
                && !transition.planned
                && transition.changed_fields.contains(&"provider".to_owned())
                && transition.changed_fields.contains(&"model".to_owned())
        })
    }));
    assert!(extensions.iter().any(|item| {
        matches!(
            item,
            TurnItem::Extension { kind, data }
                if kind == "provider_pair_switch_v1"
                    && data.get("from_provider").and_then(serde_json::Value::as_str)
                        == Some("fake-a")
                    && data.get("to_provider").and_then(serde_json::Value::as_str)
                        == Some("fake-b")
                    && data.get("why").and_then(serde_json::Value::as_str)
                        == Some("fallback_chain")
        )
    }));

    world.shutdown().await;
}

/// Core's trigger fence is authoritative: even a resolver willing to switch
/// is never consulted for a request-shape failure.
#[tokio::test]
async fn invalid_request_never_engages_the_fallback_chain() {
    let fake_a = Arc::new(FakeProvider::new(vec![FakeStep::Error {
        kind: haider_provider::ProviderErrorKind::InvalidRequest,
        message: "bad request belongs to this pair".into(),
        retry_after_ms: None,
    }]));
    let fake_b = Arc::new(FakeProvider::new(text_turn("must stay unused")));
    let world = PairSwitchWorld::boot_with_fallback(
        "fallback-invalid",
        fake_a.clone(),
        fake_b.clone(),
        true,
    )
    .await;
    let (run_id, disposition) = world
        .submit_turn(
            "fallback-invalid-turn",
            "do not switch",
            DeliveryMode::Steer,
        )
        .await;
    assert_eq!(disposition, TurnAdmissionDisposition::Started);
    world.await_run_state(&run_id, RunState::Errored).await;
    assert_eq!(fake_a.requests().len(), 1);
    assert!(fake_b.requests().is_empty());
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata")
        .expect("typed metadata");
    assert_eq!(
        (metadata.provider.as_str(), metadata.model.as_str()),
        ("fake-a", "model-a")
    );
    let model_facts = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("journal")
        .into_iter()
        .filter(|event| ModelSelected::from_payload_value(&event.payload).is_some())
        .count();
    assert_eq!(model_facts, 0);
    world.shutdown().await;
}

/// LAW (pair_switch_is_receipted_and_next_turn_resolves_the_new_provider):
/// turn 1 lands on provider A; a committed selection of the (B, model-b)
/// pair is receipted, replays idempotently, and turn 2's provider request
/// lands on B with the selected model — with no worker restart in between.
///
/// MUTATION CHECK: drop the per-turn metadata re-read (`fresh_turn_metadata`
/// → the supervisor's spawn snapshot). Expected runtime failure: turn 2
/// lands on provider A again. Executed post-commit — see the F1 mutation
/// notes.
#[tokio::test]
async fn pair_switch_is_receipted_and_next_turn_resolves_the_new_provider() {
    let fake_a = Arc::new(FakeProvider::new(text_turn("answer from a")));
    let fake_b = Arc::new(FakeProvider::new(text_turn("answer from b")));
    let world = PairSwitchWorld::boot("f1-switch", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-switch-one", "first question").await;
    assert_eq!(fake_a.requests().len(), 1, "turn 1 lands on provider A");
    assert_eq!(fake_a.requests()[0].model, "model-a");
    assert!(fake_b.requests().is_empty());

    // The switch commits between turns…
    let command = world.select_command("f1-switch-select");
    let SessionSelectModelOutcome::Committed { selected, envelope } = world
        .store
        .select_session_model(command.clone())
        .await
        .expect("select model")
    else {
        panic!("first selection must commit");
    };
    assert_eq!(selected.provider, "fake-b");
    assert_eq!(selected.model, "model-b");
    assert_eq!(envelope.seq, selected.selected_seq);
    let fact = ModelSelected::from_payload_value(&envelope.payload)
        .expect("committed envelope carries the model_selected fact");
    assert_eq!(fact.provider, "fake-b");
    assert_eq!(fact.model, "model-b");

    // …is receipted (same-command replay returns the same coordinates and
    // appends nothing)…
    let receipt = world
        .store
        .session_select_model_receipt(
            command.command_id.clone(),
            command.request_digest.clone(),
            command.request_json.clone(),
        )
        .await
        .expect("receipt lookup")
        .expect("committed receipt");
    assert_eq!(receipt, selected);
    let SessionSelectModelOutcome::IdempotentReplay { selected: replayed } = world
        .store
        .select_session_model(command)
        .await
        .expect("replay selection")
    else {
        panic!("same-command retry must replay, not re-commit");
    };
    assert_eq!(replayed, selected);
    let facts = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|event| ModelSelected::from_payload_value(&event.payload).is_some())
        .count();
    assert_eq!(facts, 1, "replay must not append a second fact");

    // …and the durable metadata is the new pair.
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.provider, "fake-b");
    assert_eq!(metadata.model, "model-b");

    // The NEXT turn resolves through the new pair: provider B's request.
    world.run_turn("f1-switch-two", "second question").await;
    assert_eq!(fake_b.requests().len(), 1, "turn 2 lands on provider B");
    assert_eq!(fake_b.requests()[0].model, "model-b");
    assert_eq!(fake_a.requests().len(), 1, "provider A saw only turn 1");
    assert_eq!(
        *world
            .cache_reconciliations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![
            (world.session_id.clone(), "fake-a".to_owned()),
            (world.session_id.clone(), "fake-b".to_owned()),
        ],
        "each ordinary turn reconciles session-owned explicit cache resources after pair resolution"
    );

    world.shutdown().await;
}

/// LAW (child_inherits_the_parents_current_pair_by_default, runtime half):
/// a child spawned AFTER a mid-session pair switch runs on the NEW pair —
/// the manifest's model_profile, the child session's metadata, and the
/// child's own provider request all say so.
///
/// MUTATION CHECK: hard-code the child's provider in the dispatcher's
/// `SpawnCoordinates` back to the supervisor snapshot. Expected runtime
/// failure: the child's request lands on provider A. Executed post-commit —
/// see the F1 mutation notes.
#[tokio::test]
async fn spawn_after_pair_switch_inherits_the_new_pair() {
    let fake_a = Arc::new(FakeProvider::new(text_turn("turn one on a")));
    let fake_b = Arc::new(FakeProvider::new(vec![
        // Parent turn 2: spawn with NO selector — pure inheritance.
        FakeStep::EmitToolCall {
            call_id: "inherit-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"inherit","prompt":"report the pair you run on"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // Child turn.
        FakeStep::EmitText {
            text: "child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        // Parent resumption with the child report.
        FakeStep::ExpectToolResult {
            call_id: "inherit-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = PairSwitchWorld::boot("f1-inherit", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-inherit-one", "warm up on a").await;
    world
        .store
        .select_session_model(world.select_command("f1-inherit-select"))
        .await
        .expect("select model");
    world.run_turn("f1-inherit-two", "now delegate").await;

    // Parent turn 2, the child's turn, and the parent resumption all landed
    // on provider B under the switched model.
    let b_requests = fake_b.requests();
    assert_eq!(b_requests.len(), 3, "provider B serves parent+child+resume");
    assert!(b_requests.iter().all(|request| request.model == "model-b"));
    assert_eq!(fake_a.requests().len(), 1, "provider A saw only turn 1");

    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read parent journal");
    let manifest = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(
        manifest.model_profile, "model-b",
        "the chip renders the child's INHERITED model"
    );
    let child_session = manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_session_id"))
        .and_then(serde_json::Value::as_str)
        .map(|id| SessionId::new(id.to_owned()))
        .expect("child session coordinates");
    let child_metadata = world
        .store
        .session_metadata(&child_session)
        .await
        .expect("child metadata read")
        .expect("child typed metadata");
    assert_eq!(child_metadata.provider, "fake-b");
    assert_eq!(child_metadata.model, "model-b");

    world.shutdown().await;
}

/// LAW (explicit_model_resolves_cross_provider_and_the_child_runs_it): a
/// parent on provider A names an explicit (model-b, fake-b) pair for its
/// child; the child's request lands on provider B while the parent stays on
/// A, and the manifest/model_profile reflects the child's model.
#[tokio::test]
async fn explicit_selector_spawns_the_child_cross_provider() {
    let fake_a = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "cross-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "cross",
                "prompt": "run on the other provider",
                "model": "model-b",
                "provider": "fake-b",
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "cross-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let fake_b = Arc::new(FakeProvider::new(text_turn("child report")));
    let world = PairSwitchWorld::boot("f1-cross", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-cross-one", "delegate across").await;

    let a_requests = fake_a.requests();
    assert_eq!(a_requests.len(), 2, "parent turn + resumption stay on A");
    assert!(a_requests.iter().all(|request| request.model == "model-a"));
    let b_requests = fake_b.requests();
    assert_eq!(b_requests.len(), 1, "the child runs on provider B");
    assert_eq!(b_requests[0].model, "model-b");

    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read parent journal");
    let manifest = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(manifest.model_profile, "model-b");
    let child_session = manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_session_id"))
        .and_then(serde_json::Value::as_str)
        .map(|id| SessionId::new(id.to_owned()))
        .expect("child session coordinates");
    let child_metadata = world
        .store
        .session_metadata(&child_session)
        .await
        .expect("child metadata read")
        .expect("child typed metadata");
    assert_eq!(child_metadata.provider, "fake-b");
    assert_eq!(child_metadata.model, "model-b");
    let parent_metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("parent metadata read")
        .expect("parent typed metadata");
    assert_eq!(parent_metadata.provider, "fake-a", "the parent never moves");
    assert_eq!(parent_metadata.model, "model-a");

    world.shutdown().await;
}

/// OWNER REGRESSION: a separator/case-fuzzy bare selector resolves against
/// the fixture inventory before establishment, and every durable/runtime
/// child coordinate receives the catalog's canonical slug.
#[tokio::test]
async fn fuzzy_bare_selector_spawns_on_the_single_canonical_catalog_row() {
    let fake_a = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "fuzzy-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "fuzzy",
                "prompt": "run on the catalog match",
                "model": "GLM4.7 flashx",
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "fuzzy-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let fake_b = Arc::new(FakeProvider::new(text_turn("child report")));
    let world = PairSwitchWorld::boot_with_catalog(
        "modelcat-fuzzy",
        fake_a.clone(),
        fake_b.clone(),
        vec![
            model_summary("fake-a", &["model-a"]),
            model_summary("fake-b", &["glm-4.7-flashx"]),
        ],
    )
    .await;

    world.run_turn("modelcat-fuzzy-turn", "delegate").await;

    let b_requests = fake_b.requests();
    assert_eq!(b_requests.len(), 1, "exactly one child request uses B");
    assert_eq!(b_requests[0].model, "glm-4.7-flashx");
    assert!(
        fake_a
            .requests()
            .iter()
            .all(|request| request.model == "model-a")
    );

    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read parent journal");
    let manifest = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
            _ => None,
        })
        .expect("spawn manifest");
    assert_eq!(manifest.model_profile, "glm-4.7-flashx");
    assert_eq!(manifest.provider(), Some("fake-b"));
    let child_session = manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_session_id"))
        .and_then(serde_json::Value::as_str)
        .map(|id| SessionId::new(id.to_owned()))
        .expect("child session coordinates");
    let child = world
        .store
        .session_metadata(&child_session)
        .await
        .expect("child metadata read")
        .expect("child metadata");
    assert_eq!(
        (child.provider.as_str(), child.model.as_str()),
        ("fake-b", "glm-4.7-flashx")
    );

    world.shutdown().await;
}

/// LAW (stale_generation_select_is_refused_and_mutates_nothing): a
/// selection carrying a stale worker generation is refused with
/// `SingleWriterViolation` and commits NOTHING — the durable metadata keeps
/// the old pair, no `model_selected` fact is appended, and the next turn
/// still lands on the old provider.
///
/// MUTATION CHECK (review-of-record RM1): delete the generation fence in
/// `Store::select_session_model`. Expected runtime failure: the stale
/// selection commits, and turn 2 lands on provider B. Executed post-commit —
/// see the F1 review mutation notes.
#[tokio::test]
async fn stale_generation_select_is_refused_and_mutates_nothing() {
    let fake_a = Arc::new(FakeProvider::new(
        [text_turn("first on a"), text_turn("second still on a")].concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("f1-stale", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-stale-one", "first question").await;

    let stale =
        world.select_command_at_generation("f1-stale-select", world.store.worker_generation() + 1);
    let error = world
        .store
        .select_session_model(stale.clone())
        .await
        .expect_err("a stale worker generation must be refused");
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);

    // Nothing committed: no receipt, no fact, unchanged durable metadata.
    let receipt = world
        .store
        .session_select_model_receipt(
            stale.command_id.clone(),
            stale.request_digest.clone(),
            stale.request_json.clone(),
        )
        .await
        .expect("receipt lookup");
    assert!(
        receipt.is_none(),
        "a refused selection must not be receipted"
    );
    let facts = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|event| ModelSelected::from_payload_value(&event.payload).is_some())
        .count();
    assert_eq!(facts, 0, "a refused selection must not append a fact");
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.provider, "fake-a");
    assert_eq!(metadata.model, "model-a");

    // The next turn still resolves through the old pair.
    world.run_turn("f1-stale-two", "second question").await;
    assert_eq!(fake_a.requests().len(), 2, "both turns land on provider A");
    assert!(fake_b.requests().is_empty(), "provider B never serves");

    world.shutdown().await;
}

/// LAW (manual_compaction_follows_the_current_selection): manual compaction
/// is provider work between turns — after a committed pair switch, the
/// summarization request lands on the NEW provider with the selected model,
/// exactly like the next turn would.
///
/// MUTATION CHECK (review-of-record RM2): feed `perform_manual_compaction`
/// the supervisor's spawn snapshot instead of `fresh_turn_metadata`.
/// Expected runtime failure: the summarization request lands on provider A.
/// Executed post-commit — see the F1 review mutation notes.
#[tokio::test]
async fn manual_compaction_follows_the_current_selection() {
    let fake_a = Arc::new(FakeProvider::new(text_turn("history built on a")));
    let fake_b = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "summary from b".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = PairSwitchWorld::boot("f1-compact", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-compact-one", "build history").await;
    world
        .store
        .select_session_model(world.select_command("f1-compact-select"))
        .await
        .expect("select model");

    // Bounded retry through the post-Done Busy window (gate27 hygiene
    // class) so the assert exercises pair pickup, not the settle race.
    let mut attempt = 0;
    loop {
        match world
            .manager
            .handle()
            .compact(
                world.session_id.clone(),
                "f1-compact-command".into(),
                world.store.worker_generation(),
                None,
            )
            .await
        {
            Ok(_) => break,
            Err(error) if error.code == ErrorCode::Busy && attempt < 40 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("manual compaction failed: {error:?}"),
        }
    }

    let b_requests = fake_b.requests();
    assert_eq!(
        b_requests.len(),
        1,
        "the summarization request lands on provider B"
    );
    assert_eq!(b_requests[0].model, "model-b");
    assert_eq!(fake_a.requests().len(), 1, "provider A saw only turn 1");

    world.shutdown().await;
}

/// A fresh manual compaction and native workflow selection have one durable
/// order. Compaction-first makes the session nonterminal and the switch is
/// Busy; switch-first commits its new pin before the compaction run appears.
#[tokio::test]
async fn manual_compaction_and_graph_switch_cannot_cross_the_idle_boundary() {
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("history for graph race"),
            vec![
                FakeStep::Delay { ms: 50 },
                FakeStep::EmitText {
                    text: "summary after ordered graph selection".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        ]
        .concat(),
    ));
    let world = PairSwitchWorld::boot(
        "compact-graph-switch",
        fake_a,
        Arc::new(FakeProvider::new(Vec::new())),
    )
    .await;
    world
        .run_turn("compact-graph-history", "build history")
        .await;

    let sink = Arc::new(GraphSelectionSink::default());
    let connection = world
        .hub
        .open_connection(
            BTreeSet::from([Capability::View, Capability::Control]),
            sink.clone(),
            ConnectionTransport::LocalSameUid,
        )
        .expect("graph control connection");
    connection
        .request(
            RequestId::new("compact-graph-attach"),
            RequestBody::SessionAttach {
                session_id: world.session_id.clone(),
                after_seq: 0,
                mode: AttachMode::Control,
                sealed_replay: false,
            },
        )
        .await
        .expect("attach graph control");
    connection
        .request(
            RequestId::new("compact-graph-pin"),
            RequestBody::GraphPin {
                command_id: CommandId::new("compact-graph-pin-command"),
                session_id: world.session_id.clone(),
                worker_generation: world.store.worker_generation(),
                template: haider_protocol::graph::SHIP_LOOP_TEMPLATE.into(),
                expected_digest: None,
            },
        )
        .await
        .expect("initial graph pin routes");
    let Some(ResponseBody::GraphPin {
        graph_id: old_graph_id,
        ..
    }) = graph_response(&sink, "compact-graph-pin")
    else {
        panic!("initial graph pin succeeds while the session is idle")
    };

    let manager = world.manager.handle();
    let compact = manager.compact(
        world.session_id.clone(),
        "compact-graph-command".into(),
        world.store.worker_generation(),
        None,
    );
    let switch = async {
        connection
            .request(
                RequestId::new("compact-graph-switch"),
                RequestBody::GraphSwitch {
                    command_id: CommandId::new("compact-graph-switch-command"),
                    session_id: world.session_id.clone(),
                    worker_generation: world.store.worker_generation(),
                    old_graph_id,
                    template: haider_protocol::graph::STAGGERED_TEMPLATE.into(),
                    expected_digest: None,
                },
            )
            .await
            .expect("graph switch routes");
        graph_response(&sink, "compact-graph-switch").expect("graph switch response")
    };
    let (accepted, switched) = tokio::join!(compact, switch);
    let accepted = accepted.expect("manual compaction remains admissible");
    match switched {
        ResponseBody::Error { code, .. } => {
            assert_eq!(
                code,
                haider_rpc::ERROR_CODE_BUSY,
                "compaction-first makes graph selection nonterminal"
            );
        }
        ResponseBody::GraphSwitch { pinned_seq, .. } => {
            assert!(
                pinned_seq < accepted.accepted_seq,
                "switch-first must commit before compaction admission"
            );
        }
        other => panic!("unexpected compact/switch race response: {other:?}"),
    }

    connection.close().await.expect("graph connection closes");
    world.shutdown().await;
}

/// LAW (submit_during_manual_compaction_queues_and_runs_after, F3): a
/// Queue-mode submission arriving while manual compaction holds the session
/// is admitted `Queued`, produces NO provider work inside the compaction
/// window, and runs to Done after the compaction lands — strictly ordered
/// behind the compaction's terminal in the journal.
#[tokio::test]
async fn submit_during_manual_compaction_queues_and_runs_after() {
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("history on a"),
            vec![
                // The held summarizer: compaction stays open ~1.5 s.
                FakeStep::Delay { ms: 1500 },
                FakeStep::EmitText {
                    text: "summary on a".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
            text_turn("queued turn answer"),
        ]
        .concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("f3-queue", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f3-queue-one", "build history").await;
    let compaction = world
        .start_compaction_and_await_window("f3-queue-compact")
        .await;
    timeout(Duration::from_secs(10), async {
        while fake_a.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("summarizer request starts inside compaction window");

    // INSIDE the window: the submission queues and no provider work leaks.
    let (queued_run, disposition) = world
        .submit_turn(
            "f3-queue-two",
            "queued while compacting",
            DeliveryMode::Queue,
        )
        .await;
    assert_eq!(
        disposition,
        TurnAdmissionDisposition::Queued,
        "a submission during compaction must queue"
    );
    assert_eq!(
        fake_a.requests().len(),
        2,
        "inside the window the provider saw turn 1 + the summarizer only"
    );

    compaction.await.expect("compaction task");
    world.await_done(&queued_run).await;

    // The queued turn ran (its request reached the provider)…
    let requests = fake_a.requests();
    assert_eq!(requests.len(), 3, "the queued turn ran after compaction");
    // …and the journal orders the compaction terminal strictly before the
    // queued turn's first non-queued activity.
    let events = world
        .store
        .read(&world.session_id, 0, 2048)
        .await
        .expect("read journal");
    let compaction_done_seq = events
        .iter()
        .filter(|event| {
            event.run_id.as_ref().is_some_and(|id| id != &queued_run)
                && event.payload.decode_event().is_ok_and(
                    |payload| matches!(payload, EventPayload::RunState(state) if state.is_terminal()),
                )
        })
        .map(|event| event.seq)
        .max()
        .expect("compaction terminal committed");
    let queued_active_seq = events
        .iter()
        .filter(|event| {
            event.run_id.as_ref() == Some(&queued_run)
                && event.payload.decode_event().is_ok_and(|payload| {
                    matches!(
                        payload,
                        EventPayload::RunState(state)
                            if !matches!(state, RunState::Queued) && !state.is_terminal()
                    )
                })
        })
        .map(|event| event.seq)
        .min()
        .expect("queued turn eventually went active");
    assert!(
        compaction_done_seq < queued_active_seq,
        "queued turn activity (seq {queued_active_seq}) must follow the \
         compaction terminal (seq {compaction_done_seq})"
    );

    world.shutdown().await;
}

/// LAW (steer_during_manual_compaction_blocks_and_never_reaches_the_summarizer,
/// F3): a Steer-mode submission during manual compaction is proper blocking —
/// its text NEVER appears in the summarization request, and it runs after the
/// compaction lands. "Queue steered but doesn't send until it's done."
#[tokio::test]
async fn steer_during_manual_compaction_blocks_and_never_reaches_the_summarizer() {
    let steer_text = "STEER_DURING_COMPACTION_c4f1";
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("history on a"),
            vec![
                FakeStep::Delay { ms: 1500 },
                FakeStep::EmitText {
                    text: "summary on a".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
            text_turn("post-compaction answer"),
        ]
        .concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("f3-steer", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f3-steer-one", "build history").await;
    let compaction = world
        .start_compaction_and_await_window("f3-steer-compact")
        .await;

    let (steer_run, disposition) = world
        .submit_turn("f3-steer-two", steer_text, DeliveryMode::Steer)
        .await;
    assert_ne!(
        disposition,
        TurnAdmissionDisposition::Started,
        "a steer during compaction must not start immediately"
    );

    compaction.await.expect("compaction task");
    world.await_done(&steer_run).await;

    let requests = fake_a.requests();
    assert_eq!(requests.len(), 3, "turn 1 + summarizer + steered turn");
    let request_text = |index: usize| -> String {
        requests[index]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.to_owned_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !request_text(1).contains(steer_text),
        "the summarization request must never carry the steered text"
    );
    assert!(
        request_text(2).contains(steer_text),
        "the steered text delivers in the first post-compaction request"
    );

    world.shutdown().await;
}

/// A fenced promotion crosses the live harness before the session actor can
/// accept a competing terminal append. The exact submitted bytes appear in
/// the next provider request once, while a response-loss retry is stale and
/// cannot enqueue a second steer.
///
/// MUTATION CHECK: remove the actor-to-supervisor promotion barrier or move
/// it after the actor completes the command. Expected runtime failure: the
/// promoted request is absent or `live_delivered` is false under the forced
/// finish race.
#[tokio::test]
async fn queue_promote_delivers_verbatim_as_exactly_one_live_steer() {
    let promoted_text = "  PROMOTED_STEER\nverbatim  ";
    let fake_a = Arc::new(FakeProvider::new(vec![
        FakeStep::Delay { ms: 1000 },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "promotion received".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("queue-promote-live", fake_a.clone(), fake_b).await;
    let (active_run, disposition) = world
        .submit_turn(
            "queue-promote-active",
            "hold the live turn",
            DeliveryMode::Queue,
        )
        .await;
    assert_eq!(disposition, TurnAdmissionDisposition::Started);
    timeout(Duration::from_secs(5), async {
        while fake_a.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active provider request starts");

    let (queued_run, disposition) = world
        .submit_turn("queue-promote-held", promoted_text, DeliveryMode::Queue)
        .await;
    assert_eq!(disposition, TurnAdmissionDisposition::Queued);
    let snapshot = world
        .hub
        .queue_snapshot(world.session_id.clone())
        .await
        .expect("held snapshot");
    let command = QueuePromoteCommand {
        session_id: world.session_id.clone(),
        id: EventId::new("queue-promote-held-user"),
        revision: snapshot.revision,
        expected_active_run_id: None,
        cancelling_event_id: EventId::new("queue-promote-live-cancelling"),
        delivery_event_id: EventId::new("queue-promote-live-delivery"),
        delta_event_id: EventId::new("queue-promote-live-delta"),
        device_id: world.device_id.clone(),
    };
    let (promoted, live_delivered) = world
        .hub
        .queue_promote_steer(command.clone())
        .await
        .expect("promotion commits and delivers");
    assert!(live_delivered, "promotion crosses the live harness barrier");
    assert_eq!(promoted.text, promoted_text);

    let stale = world
        .hub
        .queue_promote_steer(command)
        .await
        .expect_err("response-loss retry is stale");
    assert!(matches!(
        stale,
        SessionHubError::Store(ref error) if error.code == ErrorCode::RevisionConflict
    ));
    world.await_done(&active_run).await;
    world
        .await_run_state(&queued_run, RunState::Cancelled)
        .await;

    let requests = fake_a.requests();
    assert_eq!(requests.len(), 2, "promotion creates one steer round");
    let promoted_copies = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, Block::Text { text } if text == promoted_text))
        .count();
    assert_eq!(promoted_copies, 1, "provider receives verbatim text once");

    world.shutdown().await;
}

/// A crash after `Consumed` commits but before `start_turn` must resume the
/// already-owned run, not mistake its missing row for a remove/promote win.
/// This plants that exact durable prefix, then hands it to the normal queued
/// recovery path and requires provider delivery.
///
/// MUTATION CHECK: treat `queue_consume == None` as unconditional skip in the
/// supervisor. Expected runtime failure: the recovered run never reaches
/// Done and the provider receives no request.
#[tokio::test]
async fn consumed_before_start_recovers_and_delivers_after_the_crash_boundary() {
    let root = tempfile::tempdir().expect("temp profile");
    let store = SqliteStoreHandle::open(root.path()).await.expect("store");
    let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
    hub.install_creatable_providers(BTreeSet::from(["fake-a".to_owned(), "fake-b".to_owned()]))
        .expect("install creatable providers");
    let session_id = SessionId::new("queue-consume-crash-session");
    let device_id = DeviceId::new("queue-consume-crash-device");
    let generation = store.worker_generation();
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("cwd"))
        .expect("canonical cwd")
        .to_string_lossy()
        .into_owned();
    hub.create_internal_session(SessionCreateCommand {
        command_id: "create-queue-consume-crash".into(),
        request_digest: "create-queue-consume-crash-digest".into(),
        request_json: r#"{"session":"queue-consume-crash"}"#.into(),
        session_id: session_id.clone(),
        cwd,
        provider: "fake-a".into(),
        model: "model-a".into(),
        max_tokens: 4096,
        permission_overrides: None,
        effort: None,
        fast: false,
        cache_policy: Default::default(),
        system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
        event_id: EventId::new("created-queue-consume-crash"),
        device_id: device_id.clone(),
    })
    .await
    .expect("create session");

    let active_run = RunId::new("queue-consume-crash-active");
    hub.accept_internal_turn(TurnAcceptCommand {
        command_id: "accept-queue-consume-crash-active".into(),
        request_digest: "accept-queue-consume-crash-active-digest".into(),
        request_json: r#"{"turn":"active"}"#.into(),
        session_id: session_id.clone(),
        worker_generation: generation,
        run_id: active_run.clone(),
        agent_id: None,
        branch_id: None,
        text: "prior active turn".into(),
        attachments: Vec::new(),
        mode: DeliveryMode::Queue,
        queued_event_id: EventId::new("queue-consume-crash-active-queued"),
        user_event_id: EventId::new("queue-consume-crash-active-user"),
        active_event_id: EventId::new("queue-consume-crash-active-session"),
        device_id: device_id.clone(),
    })
    .await
    .expect("accept active turn");
    let state_envelope = |event_id: &str, run_id: RunId, state: RunState| EventEnvelope {
        schema_version: SCHEMA_VERSION,
        event_id: EventId::new(event_id),
        seq: 0,
        session_id: session_id.clone(),
        branch_id: None,
        run_id: Some(run_id),
        agent_id: None,
        device_id: device_id.clone(),
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
        payload: serde_json::to_value(EventPayload::RunState(state))
            .expect("state payload")
            .into(),
    };
    let mut thinking = [state_envelope(
        "queue-consume-crash-thinking",
        active_run.clone(),
        RunState::Thinking,
    )];
    hub.append(&mut thinking).await.expect("active turn starts");

    let queued_text = "deliver after consumed crash";
    let queued_run = RunId::new("queue-consume-crash-held");
    let queued = hub
        .accept_internal_turn(TurnAcceptCommand {
            command_id: "accept-queue-consume-crash-held".into(),
            request_digest: "accept-queue-consume-crash-held-digest".into(),
            request_json: r#"{"turn":"held"}"#.into(),
            session_id: session_id.clone(),
            worker_generation: generation,
            run_id: queued_run.clone(),
            agent_id: None,
            branch_id: None,
            text: queued_text.into(),
            attachments: Vec::new(),
            mode: DeliveryMode::Queue,
            queued_event_id: EventId::new("queue-consume-crash-held-queued"),
            user_event_id: EventId::new("queue-consume-crash-held-user"),
            active_event_id: EventId::new("queue-consume-crash-held-session"),
            device_id: device_id.clone(),
        })
        .await
        .expect("accept held turn");
    assert_eq!(queued.disposition, TurnAdmissionDisposition::Queued);
    store
        .queue_consume(QueueConsumeCommand {
            session_id: session_id.clone(),
            run_id: queued_run.clone(),
            delta_event_id: EventId::new("queue-consume-crash-delta"),
            device_id: device_id.clone(),
        })
        .await
        .expect("consume boundary commits")
        .expect("held row exists");
    let mut done = [state_envelope(
        "queue-consume-crash-active-done",
        active_run,
        RunState::Done,
    )];
    hub.append(&mut done).await.expect("prior run completes");

    let fake_a = Arc::new(FakeProvider::new(text_turn("recovered answer")));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let manager = WorkerManager::start(
        hub.clone(),
        WorkerDependencies {
            diagnostics: None,
            provider_factory: Arc::new(RoutingProviderFactory {
                providers: HashMap::from([
                    ("fake-a".to_owned(), fake_a.clone()),
                    ("fake-b".to_owned(), fake_b),
                ]),
                cache_reconciliations: Arc::new(Mutex::new(Vec::new())),
                fallback_enabled: false,
            }),
            tool_factory: Arc::new(BrokerToolFactory),
            delegation: None,
            web_search: None,
        },
        false,
    );
    hub.install_worker_manager(manager.handle())
        .expect("install manager");
    manager
        .handle()
        .recover_queued(queued, 0)
        .await
        .expect("recovery handoff");
    timeout(Duration::from_secs(10), async {
        loop {
            let events = store.read(&session_id, 0, 1024).await.expect("journal");
            if events.iter().any(|event| {
                event.run_id.as_ref() == Some(&queued_run)
                    && event
                        .payload
                        .decode_event()
                        .is_ok_and(|payload| payload == EventPayload::RunState(RunState::Done))
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("consumed run recovers to Done");
    let requests = fake_a.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == queued_text))
    }));

    manager.shutdown().await.expect("manager shutdown");
    hub.shutdown().await.expect("hub shutdown");
    store.close().await.expect("store close");
}

/// LAW (switch_during_manual_compaction_lands_after_it, F3): a pair switch
/// COMMITS while manual compaction is in flight (and the journal visibly says
/// `Compacting` inside the window); the compaction itself finishes on the
/// pair it started with, and the first post-compaction turn resolves through
/// the NEW pair. Directly answers the owner's "can we switch while the
/// compaction is happening?".
#[tokio::test]
async fn switch_during_manual_compaction_lands_after_it() {
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("history on a"),
            vec![
                FakeStep::Delay { ms: 1500 },
                FakeStep::EmitText {
                    text: "summary on a".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        ]
        .concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(text_turn("first answer on b")));
    let world = PairSwitchWorld::boot("f3-switch", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f3-switch-one", "build history").await;
    let compaction = world
        .start_compaction_and_await_window("f3-switch-compact")
        .await;

    // INSIDE the window: the session visibly compacts, and the switch commits
    // through the ACTOR (the real wire path — a store-direct select would
    // desync the actor's head and stage an impossible client).
    assert!(
        world
            .latest_run_states()
            .await
            .iter()
            .any(|(_, state)| matches!(state, RunState::Compacting)),
        "the session state is Compacting inside the window"
    );
    let SessionSelectModelOutcome::Committed { selected, .. } = world
        .hub
        .select_session_model(world.select_command("f3-switch-select"))
        .await
        .expect("selection during compaction must commit")
    else {
        panic!("selection during compaction must commit, not replay");
    };
    assert_eq!(selected.provider, "fake-b");

    compaction.await.expect("compaction task");

    // The compaction finished on the OLD pair (it was in-flight provider
    // work), and provider B never served the summarizer.
    assert_eq!(
        fake_a.requests().len(),
        2,
        "compaction summarized on the pair it started with"
    );
    assert_eq!(fake_a.requests()[1].model, "model-a");
    assert!(fake_b.requests().is_empty());

    // The FIRST post-compaction turn resolves through the new pair.
    world
        .run_turn("f3-switch-two", "first post-compaction turn")
        .await;
    assert_eq!(fake_b.requests().len(), 1, "next turn lands on provider B");
    assert_eq!(fake_b.requests()[0].model, "model-b");

    world.shutdown().await;
}

/// LAW (unavailable_is_typed, spawn half): naming a pair whose provider is
/// not creatable rejects with a typed, provider-readable tool result — the
/// turn CONTINUES, no child exists, and the rejection names the kind.
#[tokio::test]
async fn unavailable_spawn_selector_is_a_typed_continuable_rejection() {
    let fake_a = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "bad-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "bad",
                "prompt": "spawn on a provider that does not exist",
                "model": "model-c",
                "provider": "fake-c",
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "bad-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("f1-refused", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("f1-refused-one", "try the bad pair").await;

    let a_requests = fake_a.requests();
    assert_eq!(a_requests.len(), 2, "the rejection continues the turn");
    let resumption_carries_typed_rejection = a_requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { call_id, preview, .. }
                    if call_id == "bad-spawn"
                        && preview.contains("provider_unavailable")
                        && preview.contains("fake-c")
            )
        })
    });
    assert!(
        resumption_carries_typed_rejection,
        "the model must see the typed refusal: {a_requests:?}"
    );
    assert!(fake_b.requests().is_empty(), "no child ever ran");
    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read parent journal");
    assert!(
        !events.iter().any(|event| matches!(
            event.payload.decode_event(),
            Ok(EventPayload::AgentSpawned(_))
        )),
        "a refused selector must never create a child"
    );

    world.shutdown().await;
}

/// LAW (LE1, effort_and_fast_persist): an effort selection and a fast toggle
/// commit through the ACTOR with the `select_model` law set — the committed
/// fact rides the envelope, the receipt replays the exact coordinates
/// (appending nothing), the durable metadata carries the tuning, and a
/// stale-generation command is refused mutating nothing.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): skip the
/// `meta_json` update in `select_session_config`. Expected runtime failure:
/// the metadata assertions below read `None`/`false`.
#[tokio::test]
async fn effort_and_fast_select_are_receipted_and_persist() {
    let fake_a = Arc::new(FakeProvider::new(text_turn("answer from a")));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("g3-persist", fake_a.clone(), fake_b.clone()).await;

    // Effort commits with its fact…
    let command = world.effort_command("g3-persist-effort", Some("xhigh"));
    let SessionSelectEffortOutcome::Committed { selected, envelope } = world
        .hub
        .select_session_effort(command.clone())
        .await
        .expect("select effort")
    else {
        panic!("first effort selection must commit");
    };
    assert_eq!(selected.effort.as_deref(), Some("xhigh"));
    assert_eq!(envelope.seq, selected.selected_seq);
    let fact = EffortSelected::from_payload_value(&envelope.payload)
        .expect("committed envelope carries the effort_selected fact");
    assert_eq!(fact.effort.as_deref(), Some("xhigh"));

    // …replays idempotently under the same command id…
    let SessionSelectEffortOutcome::IdempotentReplay { selected: replayed } = world
        .hub
        .select_session_effort(command)
        .await
        .expect("replay effort selection")
    else {
        panic!("same-command retry must replay, not re-commit");
    };
    assert_eq!(replayed, selected);
    let effort_facts = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read journal")
        .into_iter()
        .filter(|event| EffortSelected::from_payload_value(&event.payload).is_some())
        .count();
    assert_eq!(effort_facts, 1, "replay must not append a second fact");

    // …the fast toggle commits with its own fact…
    let SessionSelectFastOutcome::Committed { envelope, .. } = world
        .hub
        .select_session_fast(world.fast_command("g3-persist-fast", true))
        .await
        .expect("select fast")
    else {
        panic!("fast toggle must commit");
    };
    let fact = FastModeSelected::from_payload_value(&envelope.payload)
        .expect("committed envelope carries the fast_mode_selected fact");
    assert!(fact.enabled);

    // …and the durable metadata carries BOTH tunings.
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.effort.as_deref(), Some("xhigh"));
    assert!(metadata.fast);

    // Reverting effort to the provider default persists None.
    world
        .hub
        .select_session_effort(world.effort_command("g3-persist-revert", None))
        .await
        .expect("revert effort");
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.effort, None);
    assert!(metadata.fast, "the revert touches effort only");

    // A stale worker generation is refused and mutates nothing.
    let mut stale = world.effort_command("g3-persist-stale", Some("low"));
    stale.worker_generation += 1;
    let stale_json = serde_json::json!({
        "session_id": world.session_id,
        "worker_generation": stale.worker_generation,
        "effort": "low",
    })
    .to_string();
    stale.request_digest = blake3::hash(stale_json.as_bytes()).to_hex().to_string();
    stale.request_json = stale_json;
    let error = world
        .hub
        .select_session_effort(stale)
        .await
        .expect_err("stale generation must refuse");
    let crate::session_hub::SessionHubError::Store(error) = error else {
        panic!("stale generation surfaces as a store error, got {error:?}");
    };
    assert_eq!(error.code, ErrorCode::SingleWriterViolation);
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.effort, None, "a refused selection mutates nothing");

    world.shutdown().await;
}

/// LAW (LE5, runtime half — extending switch_during_manual_compaction): an
/// effort selection COMMITS through the actor while manual compaction is in
/// flight, and the compaction still lands — the `effort_selected` fact moved
/// the journal, not the tree, so the head CAS tolerates it instead of
/// wedging.
#[tokio::test]
async fn effort_select_during_manual_compaction_lands_after_it() {
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("history on a"),
            vec![
                FakeStep::Delay { ms: 1500 },
                FakeStep::EmitText {
                    text: "summary on a".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        ]
        .concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("g3-compact", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("g3-compact-one", "build history").await;
    let compaction = world
        .start_compaction_and_await_window("g3-compact-compact")
        .await;

    assert!(
        world
            .latest_run_states()
            .await
            .iter()
            .any(|(_, state)| matches!(state, RunState::Compacting)),
        "the session state is Compacting inside the window"
    );
    let SessionSelectEffortOutcome::Committed { selected, .. } = world
        .hub
        .select_session_effort(world.effort_command("g3-compact-select", Some("max")))
        .await
        .expect("effort selection during compaction must commit")
    else {
        panic!("effort selection during compaction must commit, not replay");
    };
    assert_eq!(selected.effort.as_deref(), Some("max"));

    // The compaction still lands: the config-fact delta did not wedge the
    // head CAS.
    compaction.await.expect("compaction task");
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("typed metadata");
    assert_eq!(metadata.effort.as_deref(), Some("max"));

    world.shutdown().await;
}

/// LAW (LE6, subagent inheritance): a child spawned with NO selector
/// inherits the parent's CURRENT effort and fast tuning through the
/// metadata clone — the child session's durable metadata carries both.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): reset
/// `child.effort`/`child.fast` in `resolve_child_metadata`. Expected
/// runtime failure: the child metadata assertions below read the defaults.
#[tokio::test]
async fn spawned_child_inherits_parent_effort_and_fast() {
    let fake_a = Arc::new(FakeProvider::new(
        [
            text_turn("turn one on a"),
            vec![
                FakeStep::EmitToolCall {
                    call_id: "tuning-spawn".into(),
                    name: "spawn_subagent".into(),
                    args: serde_json::json!({
                        "task": "inherit tuning",
                        "prompt": "report the tuning you run with"
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
                    call_id: "tuning-spawn".into(),
                },
                FakeStep::EmitText {
                    text: "parent merged".into(),
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        ]
        .concat(),
    ));
    let fake_b = Arc::new(FakeProvider::new(Vec::new()));
    let world = PairSwitchWorld::boot("g3-inherit", fake_a.clone(), fake_b.clone()).await;

    world.run_turn("g3-inherit-one", "warm up").await;
    world
        .hub
        .select_session_effort(world.effort_command("g3-inherit-effort", Some("xhigh")))
        .await
        .expect("select effort");
    world
        .hub
        .select_session_fast(world.fast_command("g3-inherit-fast", true))
        .await
        .expect("select fast");
    world.run_turn("g3-inherit-two", "now delegate").await;

    let events = world
        .store
        .read(&world.session_id, 0, 1024)
        .await
        .expect("read parent journal");
    let manifest = events
        .iter()
        .find_map(|event| match event.payload.decode_event() {
            Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
            _ => None,
        })
        .expect("spawn manifest");
    let child_session = manifest
        .coordinates
        .as_ref()
        .and_then(|coordinates| coordinates.get("child_session_id"))
        .and_then(serde_json::Value::as_str)
        .map(|id| SessionId::new(id.to_owned()))
        .expect("child session coordinates");
    let child_metadata = world
        .store
        .session_metadata(&child_session)
        .await
        .expect("child metadata read")
        .expect("child typed metadata");
    assert_eq!(child_metadata.effort.as_deref(), Some("xhigh"));
    assert!(child_metadata.fast, "the child inherits the fast flag");

    world.shutdown().await;
}

/// LAW (LT3, cross-provider strip): after a model switch to a different
/// provider family, NO foreign provider-opaque facts reach the request — the
/// journaled continuation state of the OLD family (here an anthropic
/// thinking fact) is stripped at prompt assembly, while the switch-target's
/// request keeps the surrounding conversation intact.
///
/// The unit half pins the tag table and the empty-message sweep directly.
///
/// MUTATION CHECK (executed — see the G3 mutation notes): skip the strip in
/// `start_turn`. Expected runtime failure: the foreign-opaque scan below
/// finds the anthropic fact in provider B's request.
#[tokio::test]
async fn cross_provider_switch_strips_foreign_opaque_facts() {
    let fake_a = Arc::new(FakeProvider::new(vec![
        // Turn 1 on provider A mints an "anthropic"-tagged opaque fact that
        // the journal keeps as provider-opaque continuation state.
        FakeStep::EmitProviderOpaque {
            provider: "anthropic".into(),
            data: serde_json::json!({
                "type": "thinking",
                "thinking": "family-local state",
                "signature": "sig-a",
            }),
        },
        FakeStep::EmitText {
            text: "answer from a".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let fake_b = Arc::new(FakeProvider::new(text_turn("answer from b")));
    let world = PairSwitchWorld::boot("g3-strip", fake_a.clone(), fake_b.clone()).await;

    world
        .run_turn("g3-strip-one", "mint continuation state")
        .await;
    world
        .hub
        .select_session_model(world.select_command("g3-strip-select"))
        .await
        .expect("switch to provider B");
    world
        .run_turn("g3-strip-two", "run on the new family")
        .await;

    let b_requests = fake_b.requests();
    assert_eq!(b_requests.len(), 1, "turn 2 lands on provider B");
    let foreign_opaque = b_requests[0].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ProviderOpaque { provider, .. } if provider == "anthropic"
            )
        })
    });
    assert!(
        !foreign_opaque,
        "no anthropic thinking fact may reach the openai-family request: {b_requests:?}"
    );
    // The strip removes the FACT, not the conversation: provider B still
    // sees turn 1's user text and assistant answer.
    let history_survives = b_requests[0].messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == "answer from a"))
    });
    assert!(
        history_survives,
        "the surrounding history must survive the strip: {b_requests:?}"
    );

    world.shutdown().await;
}

/// LAW (LT3, unit half): the tag table maps every session provider to the
/// ONE opaque tag its wire accepts, and the strip drops foreign blocks plus
/// any message they leave empty while passing same-family blocks verbatim.
#[test]
fn opaque_tag_table_and_strip_are_exact() {
    use crate::worker::{accepted_opaque_provider, strip_foreign_provider_opaque};

    for (provider, accepted) in [
        ("anthropic", "anthropic"),
        ("anthropic-oauth", "anthropic"),
        ("openai", "openai"),
        ("openai-oauth", "openai"),
        ("gemini", "gemini"),
        ("openai-compatible", "openai-compatible"),
        ("kimi-oauth", "openai-compatible"),
        ("deepseek", "openai-compatible"),
        ("xai", "openai-compatible"),
        ("grok-oauth", "openai-compatible"),
        ("custom-lab", "openai-compatible"),
    ] {
        assert_eq!(accepted_opaque_provider(provider), accepted, "{provider}");
    }

    let anthropic_fact = Block::ProviderOpaque {
        provider: "anthropic".into(),
        data: serde_json::json!({"type": "thinking"}).into(),
    };
    let openai_fact = Block::ProviderOpaque {
        provider: "openai".into(),
        data: serde_json::json!({"type": "reasoning"}).into(),
    };
    let mut messages = vec![
        haider_provider::Message::user_text("hello"),
        haider_provider::Message::assistant(vec![openai_fact.clone()]),
        haider_provider::Message::assistant(vec![
            anthropic_fact.clone(),
            Block::Text {
                text: "kept".into(),
            },
        ]),
    ];
    strip_foreign_provider_opaque(&mut messages, "anthropic-oauth");
    assert_eq!(
        messages.len(),
        2,
        "the emptied foreign-only assistant message is swept"
    );
    assert_eq!(
        messages[1].blocks,
        vec![
            anthropic_fact,
            Block::Text {
                text: "kept".into()
            }
        ],
        "same-family facts and text pass through verbatim"
    );
}

/// Cache boundaries are coordinates in the post-strip provider projection.
/// Removing a foreign-opaque-only message remaps every later boundary while
/// leaving the surviving signed/native blocks byte-exact.
#[test]
fn cache_boundaries_remap_across_foreign_opaque_removal() {
    use crate::worker::strip_foreign_provider_opaque_projection;
    use haider_core::CompiledPromptProjection;

    let kept = Block::ProviderOpaque {
        provider: "anthropic".into(),
        data: serde_json::json!({"type": "thinking", "signature": "exact"}).into(),
    };
    let mut projection = CompiledPromptProjection {
        messages: vec![
            haider_provider::Message::user_text("summary"),
            haider_provider::Message::assistant(vec![Block::ProviderOpaque {
                provider: "openai".into(),
                data: serde_json::json!({"type": "reasoning", "encrypted": "exact"}).into(),
            }]),
            haider_provider::Message::assistant(vec![
                kept.clone(),
                Block::Text {
                    text: "stable answer".into(),
                },
            ]),
            haider_provider::Message::user_text("current"),
        ],
        stable_history_end: 3,
        current_user_start: 3,
        latest_compaction_summary_end: Some(1),
    };
    strip_foreign_provider_opaque_projection(&mut projection, "anthropic-oauth");
    assert_eq!(projection.stable_history_end, 2);
    assert_eq!(projection.current_user_start, 2);
    assert_eq!(projection.latest_compaction_summary_end, Some(1));
    assert_eq!(projection.messages.len(), 3);
    assert_eq!(projection.messages[1].blocks[0], kept);
}

/// WH4 daemon half — DeepSeek's adapter already splits prompt misses and
/// cache hits, so the core must not subtract cache reads from uncached input
/// a second time. OpenAI remains subset-shaped to make this mutation-killable.
#[test]
fn wh4_deepseek_cache_usage_is_disjoint_at_the_worker_boundary() {
    assert!(!super::cached_input_is_subset_for_provider("deepseek"));
    assert!(super::cached_input_is_subset_for_provider("openai"));
}

/// rev933b finding 7 MUTATION CHECK: drop the `expected_pair` comparison in
/// `select_session_model` (apply unconditionally). Expected RUNTIME failure:
/// the stale automatic switch overwrites the user's explicit selection
/// instead of refusing with RevisionConflict.
#[tokio::test]
async fn automatic_switch_cas_refuses_when_the_pair_moved_underneath_it() {
    let fake_a = Arc::new(FakeProvider::new(text_turn("answer from a")));
    let fake_b = Arc::new(FakeProvider::new(text_turn("answer from b")));
    let world = PairSwitchWorld::boot("f7-cas", fake_a.clone(), fake_b.clone()).await;

    // The user's explicit selection moves the durable pair to (fake-b,
    // model-b); explicit commands are unconditional (expected_pair: None).
    let explicit = world.select_command("f7-cas-explicit");
    assert!(matches!(
        world
            .store
            .select_session_model(explicit)
            .await
            .expect("explicit selection"),
        SessionSelectModelOutcome::Committed { .. }
    ));

    // An automatic switch that still believes the session runs (fake-a,
    // model-a) must refuse rather than overwrite the newer explicit word.
    let mut stale = world
        .select_command_at_generation("f7-cas-automatic-stale", world.store.worker_generation());
    stale.expected_pair = Some(("fake-a".to_owned(), "model-a".to_owned()));
    let refusal = world
        .store
        .select_session_model(stale)
        .await
        .expect_err("stale CAS must refuse");
    assert_eq!(refusal.code, ErrorCode::RevisionConflict);

    // The pair the CLI committed survives untouched.
    let metadata = world
        .store
        .session_metadata(&world.session_id)
        .await
        .expect("metadata read")
        .expect("session metadata");
    assert_eq!(metadata.provider, "fake-b");
    assert_eq!(metadata.model, "model-b");

    // A CAS that observed the CURRENT pair applies normally.
    let mut fresh = world
        .select_command_at_generation("f7-cas-automatic-fresh", world.store.worker_generation());
    fresh.expected_pair = Some(("fake-b".to_owned(), "model-b".to_owned()));
    assert!(matches!(
        world
            .store
            .select_session_model(fresh)
            .await
            .expect("fresh CAS applies"),
        SessionSelectModelOutcome::Committed { .. }
    ));
}
