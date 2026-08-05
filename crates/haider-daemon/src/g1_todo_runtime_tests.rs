//! G1 daemon-boundary laws for `todo_write`: registry advertisement and
//! routing (L1), live-turn Plan fact flow (L3), the durable Todos node (L4),
//! and the root-only tool pack (L5).

#![allow(clippy::expect_used)]

use crate::session_hub::{SessionHub, SessionHubConfig};
use crate::worker::{
    BrokerToolFactory, ProviderFactory, RegisteredToolRoute, ResolvedTurnProvider, TurnToolFactory,
    WorkerDependencies, WorkerManager, advertised_tool_definitions, registered_tool_route,
    registered_tools,
};
use haider_core::{
    SessionCreateCommand, SqliteStoreHandle, StoreHandle, TurnAcceptCommand,
    TurnAdmissionDisposition,
};
use haider_protocol::DeliveryMode;
use haider_protocol::EventPayload;
use haider_protocol::envelope::PromptRender;
use haider_protocol::error::HaiderError;
use haider_protocol::history::{NodeKind, TodoState};
use haider_protocol::ids::{DeviceId, EventId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::session::SessionMetadataV1;
use haider_protocol::state::RunState;
use haider_protocol::tool::{DispatchMode, ToolPermissionDefault};
use haider_provider::{FakeProvider, FakeStep, Provider, TurnRequest};
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
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
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
        let root = tempfile::tempdir().expect("temp profile");
        let store = SqliteStoreHandle::open(root.path()).await.expect("store");
        // The OS reclaims the leaked profile tree, matching sibling runtime
        // tests' lifetime handling.
        std::mem::forget(root);
        let hub = SessionHub::new(store.clone(), SessionHubConfig::default()).expect("hub");
        let manager = WorkerManager::start(
            hub.clone(),
            WorkerDependencies {
                provider_factory: Arc::new(FixedProviderFactory { provider }),
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
            provider: "fake".into(),
            model: "fake-model".into(),
            max_tokens: 4096,
            permission_overrides: None,
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

    async fn typed_payloads(&self) -> Vec<(EventPayload, PromptRender)> {
        self.store
            .read(&self.session_id, 0, 2048)
            .await
            .expect("read journal")
            .into_iter()
            .filter_map(|event| {
                serde_json::from_value::<EventPayload>(event.payload.clone())
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
    assert!(
        definition.description.contains("REPLACES the whole list"),
        "the description teaches whole-list-replace usage"
    );
}

/// L5 (advertisement seam): the root pack keeps `todo_write`; a delegated
/// child's pack removes EXACTLY that one tool.
/// MUTATION CHECK: stop filtering (or filter the wrong name) in
/// `advertised_tool_definitions`. Expected RUNTIME failure: the child pack
/// below still advertises todo_write, or loses a second tool.
#[test]
fn child_tool_pack_excludes_exactly_todo_write() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let root = advertised_tool_definitions(&factory, false);
    let child = advertised_tool_definitions(&factory, true);
    assert!(
        root.iter()
            .any(|definition| definition.name == "todo_write")
    );
    assert!(
        !child
            .iter()
            .any(|definition| definition.name == "todo_write")
    );
    assert_eq!(root.len(), child.len() + 1, "exactly one tool removed");
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
