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
            let (Some(run_id), Ok(EventPayload::RunState(state))) = (
                event.run_id.clone(),
                serde_json::from_value::<EventPayload>(event.payload.clone()),
            ) else {
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
                && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
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
                && serde_json::from_value::<EventPayload>(event.payload.clone()).is_ok_and(
                    |payload| {
                        matches!(
                            payload,
                            EventPayload::RunState(state)
                                if !matches!(state, RunState::Queued) && !state.is_terminal()
                        )
                    },
                )
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
                Block::Text { text } => Some(text.as_str()),
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
            serde_json::from_value::<EventPayload>(event.payload.clone()),
            Ok(EventPayload::AgentSpawned(_))
        )),
        "a refused selector must never create a child"
    );

    world.shutdown().await;
}
