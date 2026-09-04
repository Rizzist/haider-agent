#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CancelToken, DeferredTicket, DeferredToolResult, HarnessActor, HarnessConfig, MemoryStore,
    SubmitTurn, ToolDispatchResult, ToolDispatcher,
};
use haider_protocol::EventPayload;
use haider_protocol::agent::{
    AgentManifest, AgentRole, ChildReport, ChipState, Grant, Placement, ReportVerification,
};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::HaiderError;
use haider_protocol::ids::{AgentId, DeviceId, ItemId, LeaseId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::state::{RunState, WaitReason};
use haider_provider::{FakeProvider, FakeStep};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

fn manifest(agent: &str, task: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentId::new(agent),
        role: AgentRole::Subagent,
        task: task.into(),
        callsign: Some(task.to_uppercase()),
        model_profile: "fake-model".into(),
        grant: Grant {
            tools: Vec::new(),
            effect_ceiling: Vec::new(),
        },
        budget_tokens: None,
        placement: Placement::Local,
        lease: LeaseId::new(format!("lease-{agent}")),
        fencing_epoch: 1,
        attempt: 0,
        parent: None,
        coordinates: None,
        cli_scope: None,
    }
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone().into()).expect("known payload")
}

struct DeferredDispatcher {
    releases: HashMap<String, Arc<Notify>>,
    completions: HashMap<String, DeferredToolResult>,
    acknowledged: Mutex<Vec<String>>,
}

impl DeferredDispatcher {
    fn two() -> Self {
        Self {
            releases: HashMap::from([
                ("call-a".into(), Arc::new(Notify::new())),
                ("call-b".into(), Arc::new(Notify::new())),
            ]),
            completions: HashMap::from([
                (
                    "call-a".into(),
                    DeferredToolResult {
                        report: ChildReport {
                            agent: AgentId::new("agent-a"),
                            summary: "report a".into(),
                            verified: ReportVerification::Unverified,
                            workspace_revision: None,
                        },
                        chip: ChipState::Done,
                        truncated: false,
                    },
                ),
                (
                    "call-b".into(),
                    DeferredToolResult {
                        report: ChildReport {
                            agent: AgentId::new("agent-b"),
                            summary: "report b".into(),
                            verified: ReportVerification::Unverified,
                            workspace_revision: None,
                        },
                        chip: ChipState::Done,
                        truncated: false,
                    },
                ),
            ]),
            acknowledged: Mutex::new(Vec::new()),
        }
    }

    fn release(&self, call_id: &str) {
        self.releases[call_id].notify_one();
    }
}

#[async_trait]
impl ToolDispatcher for DeferredDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        let suffix = call_id.trim_start_matches("call-");
        Ok(ToolDispatchResult::Deferred(DeferredTicket {
            id: format!("ticket-{suffix}"),
            manifest: manifest(&format!("agent-{suffix}"), suffix),
        }))
    }

    async fn collect_deferred(
        &self,
        ticket: &DeferredTicket,
        _cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        let call_id = format!(
            "call-{}",
            ticket.manifest.agent.as_str().trim_start_matches("agent-")
        );
        self.releases[&call_id].notified().await;
        Ok(self.completions[&call_id].clone())
    }

    async fn acknowledge_deferred(&self, ticket: &DeferredTicket) -> Result<(), HaiderError> {
        self.acknowledged
            .lock()
            .expect("ack lock")
            .push(ticket.id.clone());
        Ok(())
    }
}

/// MUTATION CHECK: resume after the first of two deferred reports, or route
/// Waiting through Idle. Expected runtime failure: the second provider
/// request appears after `call-a`, or an Idle envelope exists between
/// Waiting(LocalChild) and Thinking.
#[tokio::test]
async fn waits_for_every_sibling_then_auto_continues_without_idle() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "call-a".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"a","prompt":"first"}),
        },
        FakeStep::EmitToolCall {
            call_id: "call-b".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"b","prompt":"second"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitText {
            text: "merged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let dispatcher = Arc::new(DeferredDispatcher::two());
    let config =
        HarnessConfig::for_session(SessionId::new("parent"), DeviceId::new("test-device"), 1, 1);
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("delegate both"))
        .await
        .expect("turn accepted");

    while !store
        .events(&SessionId::new("parent"))
        .await
        .iter()
        .any(|event| {
            matches!(
                typed(event),
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::LocalChild
                })
            )
        })
    {
        tokio::task::yield_now().await;
    }
    dispatcher.release("call-a");
    tokio::task::yield_now().await;
    assert_eq!(provider.requests().len(), 1, "first report cannot resume");
    dispatcher.release("call-b");
    let outcome = turn.wait().await.expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let tool_results = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::ToolResult {
                call_id, preview, ..
            } => Some((call_id.as_str(), preview.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_results,
        vec![
            ("call-a", "agent: agent-a\n\nreport a"),
            ("call-b", "agent: agent-b\n\nreport b")
        ]
    );

    let events = store.events(&SessionId::new("parent")).await;
    let waiting = events
        .iter()
        .position(|event| {
            matches!(
                typed(event),
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::LocalChild
                })
            )
        })
        .expect("waiting committed");
    let resumed = events
        .iter()
        .enumerate()
        .skip(waiting + 1)
        .find_map(|(index, event)| {
            matches!(typed(event), EventPayload::RunState(RunState::Thinking)).then_some(index)
        })
        .expect("thinking resumed");
    assert!(!events[waiting + 1..resumed].iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::SessionState(haider_protocol::state::SessionState::Idle { .. })
        )
    }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(typed(event), EventPayload::AgentReport(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                matches!(
                    typed(event),
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ChildResult { .. },
                        ..
                    })
                )
            })
            .count(),
        2
    );
    assert_eq!(dispatcher.acknowledged.lock().expect("ack lock").len(), 2);

    drop(handle);
    actor_task.await.expect("actor exits");
}

/// MUTATION CHECK: propagate a child failure as a parent run error instead
/// of a settled tool result. Expected runtime failure: the follow-up request
/// is absent or does not contain the child's public failure text.
#[tokio::test]
async fn errored_child_is_a_red_report_and_parent_tool_result() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "call-a".into(),
            name: "spawn_subagent".into(),
            args: serde_json::json!({"task":"a","prompt":"fail"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let mut dispatcher = DeferredDispatcher::two();
    dispatcher.completions.insert(
        "call-a".into(),
        DeferredToolResult {
            report: ChildReport {
                agent: AgentId::new("agent-a"),
                summary: "public child failure".into(),
                verified: ReportVerification::Red,
                workspace_revision: None,
            },
            chip: ChipState::Error,
            truncated: false,
        },
    );
    let dispatcher = Arc::new(dispatcher);
    let config = HarnessConfig::for_session(
        SessionId::new("error-parent"),
        DeviceId::new("test-device"),
        1,
        1,
    );
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store,
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("delegate failure"))
        .await
        .expect("turn accepted");
    tokio::task::yield_now().await;
    dispatcher.release("call-a");
    let outcome = turn.wait().await.expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(provider.requests()[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                Block::ToolResult { preview, .. } if preview == "agent: agent-a\n\npublic child failure"
            )
        })
    }));
    drop(handle);
    actor_task.await.expect("actor exits");
}
