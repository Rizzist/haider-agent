#![allow(clippy::expect_used)]

//! G1 daemon-boundary laws for `todo_write`: registry advertisement and
//! routing (L1), live-turn Plan fact flow (L3), the durable Todos node (L4),
//! and the root-only tool pack (L5).

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, RegisteredToolRoute, ResolvedTurnProvider, TurnToolFactory,
    WebCapabilityDegrade, WorkerDependencies, WorkerManager, advertised_tool_definitions,
    registered_tool_route, registered_tools,
};
use haider_core::{
    GraphPinCommand, MenuResolutionCommand, SessionCreateCommand, SqliteStoreHandle, StoreHandle,
    TurnAcceptCommand, TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::PromptRender;
use haider_protocol::error::HaiderError;
use haider_protocol::graph::SHIP_LOOP_TEMPLATE;
use haider_protocol::history::{NodeKind, TodoState};
use haider_protocol::ids::{DeviceId, EventId, GraphId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::menu::{AnswerVia, MenuAnswer, MenuKind};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_protocol::tool::{DispatchMode, ToolPermissionDefault};
use haider_provider::{FakeProvider, FakeStep, MessageRole, Provider, TurnRequest};
use std::sync::Arc;
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
}

impl World {
    async fn boot(prefix: &str, provider: Arc<FakeProvider>) -> Self {
        Self::boot_with_scope(
            prefix,
            provider,
            vec!["graph_evidence".into(), "spawn_subagent".into()],
            false,
        )
        .await
    }

    async fn boot_with_scope(
        prefix: &str,
        provider: Arc<FakeProvider>,
        tools: Vec<String>,
        allow_exec: bool,
    ) -> Self {
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        // The OS reclaims the leaked profile tree, matching sibling runtime
        // tests' lifetime handling.
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                diagnostics: None,
                provider_factory: Arc::new(FixedProviderFactory { provider }),
                // This suite scripts graph/delegation operations. Its scope
                // is explicit; coding discovery has dedicated runtime pins.
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

    async fn run_turn_with_explicit_graph_abandon(&self, label: &str, text: &str) -> RunId {
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
        self.manager
            .handle()
            .submit(accepted)
            .await
            .expect("submit turn");
        let (menu, request_seq) = timeout(Duration::from_secs(10), async {
            loop {
                let events = self
                    .store
                    .read(&self.session_id, 0, 2048)
                    .await
                    .expect("read journal");
                if let Some(opening) = events.into_iter().find(|event| {
                    event.run_id.as_ref() == Some(&run_id)
                        && event.payload.decode_event().is_ok_and(|payload| {
                            matches!(
                                payload,
                                EventPayload::MenuOpened(ref menu)
                                    if matches!(menu.kind, MenuKind::GraphAbandonConfirm { .. })
                            )
                        })
                }) {
                    let EventPayload::MenuOpened(menu) =
                        serde_json::from_value(opening.payload.into()).expect("typed menu")
                    else {
                        unreachable!();
                    };
                    break (menu, opening.seq);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("guard opens abandonment confirmation");
        self.hub
            .resolve_hook_menu(MenuResolutionCommand {
                command_id: format!("abandon-{label}"),
                session_id: self.session_id.clone(),
                request_seq,
                worker_generation: self.store.worker_generation(),
                allow_prior_generation: false,
                answer: MenuAnswer {
                    menu: menu.id,
                    option_key: Some("abandon-and-finish".into()),
                    option_index: 1,
                    value: None,
                    via: AnswerVia::Rpc,
                },
                device_id: self.device_id.clone(),
                input_is_secret_reference: false,
            })
            .await
            .expect("explicit graph abandonment commits");
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run reaches Done");
    }

    async fn typed_payloads(&self) -> Vec<(EventPayload, PromptRender)> {
        self.store
            .read(&self.session_id, 0, 2048)
            .await
            .expect("read journal")
            .into_iter()
            .filter_map(|event| {
                event
                    .payload
                    .decode_event()
                    .ok()
                    .map(|payload| (payload, event.render.prompt))
            })
            .collect()
    }
}

fn todo_call(call_id: &str, items: serde_json::Value) -> [FakeStep; 3] {
    [
        FakeStep::EmitToolCall {
            call_id: call_id.into(),
            name: "todo_write".into(),
            args: serde_json::json!({ "items": items }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
    ]
}

fn plan_facts(payloads: &[(EventPayload, PromptRender)]) -> Vec<&ItemEvent> {
    payloads
        .iter()
        .filter_map(|(payload, _)| match payload {
            EventPayload::Item(event)
                if matches!(
                    event,
                    ItemEvent::Started {
                        item: TurnItem::Plan { .. },
                        ..
                    } | ItemEvent::Completed {
                        item: TurnItem::Plan { .. },
                        ..
                    }
                ) =>
            {
                Some(event)
            }
            _ => None,
        })
        .collect()
}

/// CG-M1 LAW: the daemon reduces graph journal truth at worker startup and
/// injects the bounded brief into the provider-only tail. The same text is
/// absent from durable event payloads.
#[tokio::test]
async fn active_graph_brief_reaches_the_provider_but_not_durable_history() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = World::boot("graph-brief-runtime", provider.clone()).await;
    world
        .hub
        .pin_graph(GraphPinCommand {
            command_id: "pin-graph-brief-runtime".into(),
            request_digest: "pin-graph-brief-runtime-digest".into(),
            request_json: r#"{"template":"ship-loop"}"#.into(),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            graph_id: GraphId::new("graph-brief-runtime"),
            template: SHIP_LOOP_TEMPLATE.into(),
            device_id: world.device_id.clone(),
        })
        .await
        .expect("pin graph");
    world
        .run_turn_with_explicit_graph_abandon("graph-brief-runtime", "continue the work")
        .await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].messages.iter().any(|message| {
        matches!(
            message.blocks.as_slice(),
            [Block::Text { text }]
                if text.starts_with("GraphBrief: BUILD attempt 1/8;")
        )
    }));
    let events = world
        .store
        .read(&world.session_id, 0, 2048)
        .await
        .expect("journal");
    assert!(
        events
            .iter()
            .all(|event| !event.payload.to_string().contains("GraphBrief:"))
    );
}

/// CG-M1 runtime law: the advertised model tool can only testify. The
/// dispatcher stamps durable evidence and the daemon-generated gate facts
/// move BUILD to VERIFY; no successor or attempt is accepted from tool args.
#[tokio::test]
async fn graph_evidence_tool_dispatches_to_daemon_gate_authority() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "graph-build-green".into(),
            name: "graph_evidence".into(),
            args: serde_json::json!({
                "node": "BUILD",
                "verdict": "green",
                "detail": "cargo build passed"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "graph-build-green".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = World::boot("graph-evidence-runtime", provider.clone()).await;
    world
        .hub
        .pin_graph(GraphPinCommand {
            command_id: "pin-graph-evidence-runtime".into(),
            request_digest: "pin-graph-evidence-runtime-digest".into(),
            request_json: r#"{"template":"ship-loop"}"#.into(),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            graph_id: GraphId::new("graph-evidence-runtime"),
            template: SHIP_LOOP_TEMPLATE.into(),
            device_id: world.device_id.clone(),
        })
        .await
        .expect("pin graph");
    let run_id = world
        .run_turn_with_explicit_graph_abandon("graph-evidence-runtime", "build and record evidence")
        .await;
    let status = world
        .hub
        .graph_status(&world.session_id)
        .await
        .expect("status")
        .expect("graph");
    assert_eq!(
        status.current_node,
        Some(haider_protocol::graph::verify_node())
    );
    assert_eq!(status.attempt, 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let expected_nodes = ["BUILD", "VERIFY", "VERIFY"];
    for (round, request) in requests.iter().enumerate() {
        let user_turn = request
            .messages
            .iter()
            .position(|message| {
                message.role == MessageRole::User
                    && matches!(message.blocks.as_slice(), [Block::Text { text }] if text == "build and record evidence")
            })
            .expect("accepted user turn is provider-visible");
        let graph_briefs = request
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                if message.role != MessageRole::User {
                    return None;
                }
                match message.blocks.as_slice() {
                    [Block::Text { text }] if text.starts_with("GraphBrief:") => {
                        Some((index, text.to_owned_string()))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            graph_briefs.len(),
            1,
            "request round {} contains exactly one current graph snapshot",
            round + 1
        );
        assert!(
            graph_briefs[0].1.starts_with(&format!(
                "GraphBrief: {} attempt 1/8;",
                expected_nodes[round]
            )),
            "request round {} reflects the graph transition at its logical request boundary",
            round + 1,
        );
        assert!(
            graph_briefs[0].0 < user_turn,
            "request round {} places the current graph snapshot before the accepted user",
            round + 1
        );
    }
    let payloads = world.typed_payloads().await;
    assert!(payloads.iter().any(|(payload, _)| {
        matches!(
            payload,
            EventPayload::EvidenceRecorded(recorded)
                if recorded.node == haider_protocol::graph::build_node()
                    && matches!(
                        &recorded.source,
                        haider_protocol::graph::GraphEvidenceSource::Model {
                            run_id: source_run,
                            call_id
                        } if source_run == &run_id && call_id == "graph-build-green"
                    )
        )
    }));
}

/// Economydiet: daemon-verified slots remain usable after receipts leave the
/// model context, and journal reconstruction uses the same slim projection.
#[tokio::test]
async fn verified_slot_resolves_journal_provenance_with_slim_live_and_replayed_results() {
    let mut script = Vec::new();
    for (id, name, args) in [
        (
            "discover-evidence",
            "list_tools",
            serde_json::json!({"filter":"graph_evidence"}),
        ),
        (
            "build-evidence",
            "graph_evidence",
            serde_json::json!({"node":"BUILD", "verdict":"green", "detail":"implementation ready"}),
        ),
        (
            "verify-command",
            "process_exec",
            serde_json::json!({"command":"echo verified"}),
        ),
        (
            "verify-evidence",
            "graph_evidence",
            serde_json::json!({"node":"VERIFY", "slot":"tests", "verdict":"green", "detail":"verification succeeded", "evidence_from":"latest_process"}),
        ),
    ] {
        script.extend([
            FakeStep::EmitToolCall {
                call_id: id.into(),
                name: name.into(),
                args,
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult { call_id: id.into() },
        ]);
    }
    script.extend((0..3).map(|_| FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }));
    let provider = Arc::new(FakeProvider::new(script));
    let world =
        World::boot_with_scope("economydiet-evidence", provider.clone(), Vec::new(), true).await;
    world
        .hub
        .pin_graph(GraphPinCommand {
            command_id: "pin-economydiet-evidence".into(),
            request_digest: "pin-economydiet-evidence".into(),
            request_json: r#"{"template":"ship-loop"}"#.into(),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            graph_id: GraphId::new("economydiet-evidence"),
            template: SHIP_LOOP_TEMPLATE.into(),
            device_id: world.device_id.clone(),
        })
        .await
        .expect("pin graph");
    let run = world
        .run_turn_with_explicit_graph_abandon("economydiet-evidence", "verify the implementation")
        .await;
    let payloads = world.typed_payloads().await;
    assert!(payloads.iter().any(|(payload, _)| matches!(payload,
        EventPayload::EvidenceRecorded(recorded) if recorded.slot.as_deref() == Some("tests")
        && matches!(&recorded.source, haider_protocol::graph::GraphEvidenceSource::ProcessSignal {run_id, call_id, ..}
            if run_id == &run && call_id == "verify-command"))));
    let journal_result = payloads
        .iter()
        .find_map(|(payload, _)| match payload {
            EventPayload::ToolResult { call_id, result } if call_id == "verify-command" => {
                Some(result)
            }
            _ => None,
        })
        .expect("durable process result");
    assert!(journal_result.preview.contains("process_signal"));
    assert!(journal_result.preview.contains("subject_digest"));
    world
        .run_turn("economydiet-evidence-replay", "recall the result")
        .await;
    let requests = provider.requests();
    let model_results = requests
        .iter()
        .flat_map(|request| &request.messages)
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::ToolResult {
                call_id, preview, ..
            } if call_id == "verify-command" => Some(preview),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(model_results.len() >= 2);
    for preview in &model_results {
        assert!(preview.contains("verified"));
        assert!(!preview.contains("process_signal"));
        assert!(!preview.contains("subject_digest"));
        assert_eq!(preview, &model_results[0], "first-send/replay model bytes");
    }
}

/// CG-M2c LAW: outstanding VERIFY testimony remains graph-local, but the
/// provider turn reaches Done only after an explicit guardrail exit.
#[tokio::test]
async fn outstanding_verify_evidence_allows_a_normal_provider_turn_to_finish() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = World::boot("graph-verify-nonblocking", provider).await;
    world
        .hub
        .pin_graph(GraphPinCommand {
            command_id: "pin-graph-verify-nonblocking".into(),
            request_digest: "pin-graph-verify-nonblocking-digest".into(),
            request_json: r#"{"template":"ship-loop"}"#.into(),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            graph_id: GraphId::new("graph-verify-nonblocking"),
            template: SHIP_LOOP_TEMPLATE.into(),
            device_id: world.device_id.clone(),
        })
        .await
        .expect("pin graph");
    world
        .hub
        .record_graph_evidence(haider_core::GraphEvidenceCommand {
            command_id: "graph-verify-nonblocking-build".into(),
            request_digest: "graph-verify-nonblocking-build-digest".into(),
            request_json: r#"{"node":"BUILD","verdict":"green"}"#.into(),
            session_id: world.session_id.clone(),
            worker_generation: world.store.worker_generation(),
            run_id: RunId::new("prior-build-run"),
            call_id: "prior-build-call".into(),
            graph_id: GraphId::new("graph-verify-nonblocking"),
            node: haider_protocol::graph::build_node(),
            verdict: haider_protocol::graph::EvidenceVerdict::Green,
            detail: "build passed".into(),
            slot: None,
            subject_digest: None,
            signal: None,
            workspace_mutation: None,
            child_contract: None,
            device_id: world.device_id.clone(),
        })
        .await
        .expect("BUILD advances to VERIFY");
    let run_id = world
        .run_turn_with_explicit_graph_abandon(
            "graph-verify-nonblocking",
            "ordinary interactive followup",
        )
        .await;
    let payloads = world.typed_payloads().await;
    assert!(
        payloads
            .iter()
            .any(|(payload, _)| { matches!(payload, EventPayload::RunState(RunState::Done)) })
    );
    assert!(!payloads.iter().any(|(payload, _)| {
        matches!(payload, EventPayload::RunState(RunState::Waiting { .. }))
    }));
    let events = world
        .store
        .read(&world.session_id, 0, 2048)
        .await
        .expect("journal");
    assert!(events.iter().any(|event| {
        event.run_id.as_ref() == Some(&run_id)
            && matches!(
                event.payload.decode_event(),
                Ok(EventPayload::RunState(RunState::Done))
            )
    }));
}

/// L1 registry: `todo_write` is one canonical registry entry — advertised in
/// provider definitions, resolvable by name to its typed route, and policy
/// `NotApplicable` (no brokered effect), awaited like `request_input`.
/// MUTATION CHECK: drop the registry entry (or its route). Expected RUNTIME
/// failure: the advertisement, the route lookup, or the manifest contract
/// assertions below fail.
#[test]
fn todo_write_is_registered_advertised_and_routable() {
    let registry = registered_tools();
    let entry = registry
        .iter()
        .find(|entry| entry.manifest.name == "todo_write")
        .expect("todo_write is registered");
    assert_eq!(entry.default, ToolPermissionDefault::NotApplicable);
    assert!(entry.manifest.effects.is_empty());
    assert_eq!(entry.manifest.dispatch, DispatchMode::Await);
    assert_eq!(entry.route, RegisteredToolRoute::TodoWrite);
    assert_eq!(
        registered_tool_route("todo_write"),
        Some(RegisteredToolRoute::TodoWrite)
    );
    let advertised = TurnToolFactory::definitions(&BrokerToolFactory);
    let definition = advertised
        .iter()
        .find(|definition| definition.name == "todo_write")
        .expect("todo_write is advertised to providers");
    // Instruct pipe: the wire definition is description-free; the whole-list-
    // replace teaching lives in the authored manifest (and, at turn time, the
    // system-prompt tool manual), not on the advertised ToolDefinition.
    assert!(
        definition
            .description
            .contains("REPLACE the whole todo list")
    );
    assert!(
        entry
            .manifest
            .description
            .contains("REPLACES the whole list"),
        "the authored manifest teaches whole-list-replace usage"
    );
}

/// L5/CG-M1 (advertisement seam): the root pack keeps `todo_write` and
/// `graph_evidence`; CU-2 also keeps native computer control out of the
/// default delegated pack, so a child loses exactly these three authorities.
/// MUTATION CHECK: stop filtering (or filter the wrong name) in
/// `advertised_tool_definitions`. Expected RUNTIME failure: the child pack
/// below still advertises todo_write, or loses a second tool.
#[test]
fn child_tool_pack_excludes_root_authority_tools() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, None, "fake", WebCapabilityDegrade::default());
    let child = advertised_tool_definitions(
        &factory,
        Some(&crate::worker::default_child_grant()),
        "fake",
        WebCapabilityDegrade::default(),
    );
    assert!(
        root.iter()
            .any(|definition| definition.name == "todo_write")
    );
    assert!(
        !child
            .iter()
            .any(|definition| definition.name == "todo_write")
    );
    assert!(
        !child
            .iter()
            .any(|definition| definition.name == "graph_evidence")
    );
    assert!(!child.iter().any(|definition| definition.name == "computer"));
    assert!(
        !child
            .iter()
            .any(|definition| definition.name == "loom_register")
    );
    assert_eq!(
        root.len(),
        child.len() + 5,
        "exactly five tools removed (todo_write, graph_evidence, computer, plan, loom_register)"
    );
    for definition in &child {
        assert!(
            root.iter().any(|other| other.name == definition.name),
            "child pack is a subset of the root pack"
        );
    }
    assert!(
        child
            .iter()
            .any(|definition| definition.name == "spawn_subagent"),
        "children retain the spawn tool (W6c)"
    );
}

/// L3 daemon runtime: a scripted provider calls `todo_write` twice in one
/// live daemon run — the journal shows Started{Plan} then Completed{Plan}
/// under the SAME item id, the second list replacing the first, and both
/// ToolResult facts carry verbatim prompt render.
/// MUTATION CHECK: route todo_write into the broker match arm in
/// `BrokerToolDispatcher::execute` instead of the actor seam. Expected
/// RUNTIME failure: the turn errors (`not dispatched by the general-tool
/// match`) and no Plan facts appear below.
#[tokio::test]
async fn two_live_todo_writes_share_one_plan_item_and_replay_results() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "live-plan-1",
        serde_json::json!([
            { "id": 0, "text": "map the seams", "state": "processing" },
            { "id": 1, "text": "wire the registry", "state": "listed", "dep": 0 },
        ]),
    ));
    script.extend(todo_call(
        "live-plan-2",
        serde_json::json!([
            { "id": 0, "text": "map the seams", "state": "completed" },
            { "id": 1, "text": "wire the registry", "state": "processing", "dep": 0 },
        ]),
    ));
    script.extend([
        FakeStep::EmitText {
            text: "progressing".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let world = World::boot("g1-live-plan", Arc::new(FakeProvider::new(script))).await;
    world.run_turn("g1-live-plan", "plan the work").await;
    let payloads = world.typed_payloads().await;
    let plans = plan_facts(&payloads);
    assert_eq!(plans.len(), 2, "one Started, one Completed");
    let ItemEvent::Started {
        item_id: started_id,
        item: TurnItem::Plan { items: first },
    } = plans[0]
    else {
        panic!("first plan fact must be Started, got {:?}", plans[0]);
    };
    let ItemEvent::Completed {
        item_id: completed_id,
        item: TurnItem::Plan { items: second },
    } = plans[1]
    else {
        panic!("second plan fact must be Completed, got {:?}", plans[1]);
    };
    assert_eq!(started_id, completed_id, "one plan item id for the run");
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].state, TodoState::Processing);
    assert_eq!(second[0].state, TodoState::Completed);
    assert_eq!(second[1].state, TodoState::Processing);
    let tool_results: Vec<_> = payloads
        .iter()
        .filter_map(|(payload, render)| match payload {
            EventPayload::ToolResult { call_id, result } => Some((call_id, result, *render)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);
    for (call_id, result, render) in tool_results {
        assert!(call_id.starts_with("live-plan-"));
        assert_eq!(render, PromptRender::Verbatim);
        assert!(result.preview.contains("\"ok\":true"));
    }
}

/// L4 daemon runtime: an all-completed `todo_write` commits the durable
/// `NodeKind::Todos` node — asserted on the journaled tree, not the panel.
/// MUTATION CHECK: drop the Plan arm from core `commit_item` node pairing.
/// Expected RUNTIME failure: no Todos node in the live journal below.
#[tokio::test]
async fn completed_plan_commits_a_todos_node_in_the_live_tree() {
    let mut script = Vec::new();
    script.extend(todo_call(
        "live-done-1",
        serde_json::json!([
            { "id": 0, "text": "ship the fix", "state": "processing" },
        ]),
    ));
    script.extend(todo_call(
        "live-done-2",
        serde_json::json!([
            { "id": 0, "text": "ship the fix", "state": "completed" },
        ]),
    ));
    script.extend([FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    let world = World::boot("g1-live-done", Arc::new(FakeProvider::new(script))).await;
    world.run_turn("g1-live-done", "finish the plan").await;
    let payloads = world.typed_payloads().await;
    let todos_nodes: Vec<_> = payloads
        .iter()
        .filter_map(|(payload, _)| match payload {
            EventPayload::NodeCommitted(node) => match &node.kind {
                NodeKind::Todos { items } => Some(items.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(todos_nodes.len(), 1, "exactly one Todos node");
    assert_eq!(todos_nodes[0].len(), 1);
    assert_eq!(todos_nodes[0][0].text, "ship the fix");
    assert_eq!(todos_nodes[0][0].state, TodoState::Completed);
    // The plan item id closed with the completion — pinned panel unpins.
    let plans = plan_facts(&payloads);
    assert_eq!(plans.len(), 2);
    let ids: Vec<&ItemId> = plans
        .iter()
        .map(|event| match event {
            ItemEvent::Started { item_id, .. } | ItemEvent::Completed { item_id, .. } => item_id,
            ItemEvent::Delta { item_id, .. } => item_id,
        })
        .collect();
    assert_eq!(ids[0], ids[1]);
}

/// L5 daemon runtime: a live delegated child's provider request advertises
/// NO `todo_write` while the root parent's does — asserted on the recorded
/// `TurnRequest.tools`, the definitive provider-facing pack.
/// MUTATION CHECK: pass `delegated_child: false` (or drop the filter) at the
/// `start_turn` tool seam. Expected RUNTIME failure: the child request below
/// advertises todo_write.
#[tokio::test]
async fn live_child_session_pack_excludes_todo_write() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "g1-spawn".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({
                "task": "focused checks",
                "prompt": "g1-child-prompt: run the focused checks"
            }),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        // Child turn consumes the next segment.
        FakeStep::EmitText {
            text: "child finished the checks".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        // Parent resumes with the child report.
        FakeStep::ExpectToolResult {
            call_id: "g1-spawn".into(),
        },
        FakeStep::EmitText {
            text: "parent merged the child report".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let world = World::boot("g1-child-pack", provider.clone()).await;
    world.run_turn("g1-child-pack", "delegate the checks").await;
    let requests = provider.requests();
    let mentions = |request: &TurnRequest, needle: &str| {
        request.messages.iter().any(|message| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if text.contains(needle)))
        })
    };
    let parent_requests: Vec<_> = requests
        .iter()
        .filter(|request| mentions(request, "delegate the checks"))
        .collect();
    let child_requests: Vec<_> = requests
        .iter()
        .filter(|request| mentions(request, "g1-child-prompt"))
        .collect();
    assert!(!parent_requests.is_empty(), "parent request recorded");
    assert!(!child_requests.is_empty(), "child request recorded");
    for request in parent_requests {
        assert!(
            request.tools.iter().any(|tool| tool.name == "todo_write"),
            "the root session advertises todo_write"
        );
    }
    for request in child_requests {
        assert!(
            !request.tools.iter().any(|tool| tool.name == "todo_write"),
            "a delegated child must not see todo_write"
        );
        assert!(
            request
                .tools
                .iter()
                .any(|tool| tool.name == "spawn_subagent"),
            "children retain the spawn tool (W6c)"
        );
    }
}
