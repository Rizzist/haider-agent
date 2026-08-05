//! Runtime laws for provider-agnostic model selection (F1): the committed
//! pair is what the NEXT logical turn — and every child spawned from it —
//! resolves through. Two recording fake providers make the landing provider
//! an asserted fact, not an inference.

#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, ResolvedTurnProvider, WorkerDependencies, WorkerManager,
};
use haider_core::{
    SessionCreateCommand, SessionSelectModelCommand, SessionSelectModelOutcome, SqliteStoreHandle,
    StoreHandle, TurnAcceptCommand, TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{DeviceId, EventId, RunId, SessionId};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::session::{ModelSelected, SessionMetadataV1};
use haider_protocol::state::RunState;
use haider_provider::{FakeProvider, FakeStep};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use tokio::time::{Duration, timeout};

/// Routes each turn to the fake registered for `metadata.provider` — the
/// injected analog of per-provider adapters, echoing the R6 contract
/// (`provider_name` mirrors the session metadata).
struct RoutingProviderFactory {
    providers: HashMap<String, Arc<FakeProvider>>,
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
        Ok(ResolvedTurnProvider {
            provider: Arc::clone(provider) as Arc<dyn haider_provider::Provider>,
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

struct PairSwitchWorld {
    store: SqliteStoreHandle,
    hub: SessionHub,
    manager: WorkerManager,
    session_id: SessionId,
    device_id: DeviceId,
}

impl PairSwitchWorld {
    /// Store + hub + manager with fakes `fake-a`/`fake-b` creatable, and one
    /// session created on the (`fake-a`, `model-a`) pair.
    async fn boot(prefix: &str, fake_a: Arc<FakeProvider>, fake_b: Arc<FakeProvider>) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        // Leak the tempdir handle so the profile outlives this constructor;
        // the OS reclaims the tree, matching sibling runtime tests' lifetime.
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        hub.install_creatable_providers(BTreeSet::from(["fake-a".to_owned(), "fake-b".to_owned()]))
            .expect("install creatable providers");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                provider_factory: Arc::new(RoutingProviderFactory {
                    providers: HashMap::from([
                        ("fake-a".to_owned(), fake_a),
                        ("fake-b".to_owned(), fake_b),
                    ]),
                }),
                tool_factory: Arc::new(BrokerToolFactory),
                delegation: None,
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
        self.await_done(&run_id).await;
        run_id
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
    }

    /// The RESOLVED (`fake-b`, `model-b`) selection command with a stable
    /// receipt identity.
    fn select_command(&self, command_id: &str) -> SessionSelectModelCommand {
        let request_json = serde_json::json!({
            "session_id": self.session_id,
            "worker_generation": self.store.worker_generation(),
            "model": "model-b",
            "provider": "fake-b",
        })
        .to_string();
        SessionSelectModelCommand {
            command_id: command_id.to_owned(),
            request_digest: blake3::hash(request_json.as_bytes()).to_hex().to_string(),
            request_json,
            session_id: self.session_id.clone(),
            worker_generation: self.store.worker_generation(),
            provider: "fake-b".into(),
            model: "model-b".into(),
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
        .find_map(
            |event| match serde_json::from_value::<EventPayload>(event.payload.clone()) {
                Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
                _ => None,
            },
        )
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
        .find_map(
            |event| match serde_json::from_value::<EventPayload>(event.payload.clone()) {
                Ok(EventPayload::AgentSpawned(manifest)) => Some(manifest),
                _ => None,
            },
        )
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
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::AgentSpawned(_))
        )),
        "a refused selector must never create a child"
    );

    world.shutdown().await;
}
