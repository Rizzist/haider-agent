#![allow(clippy::expect_used)]
use async_trait::async_trait;
use haider_core::{
    CancelToken, DeferredTicket, DeferredToolResult, HarnessActor, HarnessConfig, MemoryStore,
    SubmitTurn, ToolDispatchResult, ToolDispatcher,
};
use haider_protocol::{
    EventPayload,
    agent::{AgentManifest, ChildReport, ChipState, ReportVerification},
    error::HaiderError,
    headless::AgentSpawnSpecV1,
    ids::{DeviceId, ItemId, RunId, SessionId},
    item::{ItemEvent, TurnItem},
    state::RunState,
};
use haider_provider::FakeProvider;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ChildDispatcher {
    calls: AtomicUsize,
    acknowledgements: AtomicUsize,
    cancellations: AtomicUsize,
    red: bool,
}
#[async_trait]
impl ToolDispatcher for ChildDispatcher {
    async fn execute(
        &self,
        _: &RunId,
        _: &ItemId,
        _: &str,
        name: &str,
        args: serde_json::Value,
        _: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        assert_eq!(name, "spawn_subagent");
        assert_eq!(args["prompt"], "bounded task");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let manifest: AgentManifest = serde_json::from_value(serde_json::json!({
            "agent":"direct-child", "role":"subagent", "task":"test", "model_profile":"fake",
            "grant":{"tools":[],"effect_ceiling":[]}, "placement":{"placement":"local"},
            "lease":"direct-lease", "fencing_epoch":1, "attempt":0
        }))
        .expect("manifest");
        Ok(ToolDispatchResult::Deferred(DeferredTicket {
            id: "direct-child".into(),
            manifest,
        }))
    }
    async fn collect_deferred(
        &self,
        ticket: &DeferredTicket,
        cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        if cancel.is_cancelled() {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(DeferredToolResult {
            report: ChildReport {
                agent: ticket.manifest.agent.clone(),
                summary: "child result".into(),
                verified: if self.red {
                    ReportVerification::Red
                } else {
                    ReportVerification::Unverified
                },
                workspace_revision: None,
            },
            chip: if self.red {
                ChipState::Error
            } else {
                ChipState::Done
            },
            truncated: false,
            truncation: None,
        })
    }
    async fn acknowledge_deferred(&self, _: &DeferredTicket) -> Result<(), HaiderError> {
        self.acknowledgements.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn direct_spawn(red: bool, refuse_done: bool) {
    let session = SessionId::new("direct-parent");
    let mut config = HarnessConfig::for_session(session.clone(), DeviceId::new("test"), 0, 1);
    config.agent_spawn = Some(AgentSpawnSpecV1 {
        task: "test".into(),
        prompt: "bounded task".into(),
        model: None,
        provider: None,
        agent_type: None,
        workflow: None,
        workflow_trigger: None,
    });
    if refuse_done {
        config.finalization_guard = Some(Arc::new(RefuseDone));
    }
    let provider = Arc::new(FakeProvider::new(vec![]));
    let store = Arc::new(MemoryStore::new());
    let dispatcher = Arc::new(ChildDispatcher {
        calls: AtomicUsize::new(0),
        acknowledgements: AtomicUsize::new(0),
        cancellations: AtomicUsize::new(0),
        red,
    });
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let task = tokio::spawn(actor.run());
    let result = handle
        .submit_turn(SubmitTurn::new("operator spawn"))
        .await
        .expect("submit")
        .wait()
        .await
        .expect("terminal");
    assert_eq!(
        result.state,
        if red || refuse_done {
            RunState::Errored
        } else {
            RunState::Done
        }
    );
    assert!(
        provider.requests().is_empty(),
        "direct parent must never ask the provider to delegate or summarize"
    );
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(dispatcher.acknowledgements.load(Ordering::SeqCst), 1);
    let events = store.events(&session).await;
    let results = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload.decode_event(),
                Ok(EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ChildResult { .. },
                    ..
                }))
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    let terminal = events.iter().find(|event| matches!(event.payload.decode_event(), Ok(EventPayload::RunState(state)) if state.is_terminal())).expect("terminal event");
    assert!(results[0].seq < terminal.seq);
    handle.stop().await.expect("stop");
    task.await.expect("actor task");
}

#[tokio::test]
async fn public_spawn_commits_one_child_result_without_a_parent_provider_request() {
    direct_spawn(false, false).await;
}

#[tokio::test]
async fn public_spawn_red_child_result_is_a_failed_operation_without_provider_work() {
    direct_spawn(true, false).await;
}

#[derive(Debug)]
struct RefuseDone;
#[async_trait]
impl haider_core::FinalizationGuard for RefuseDone {
    async fn before_done(
        &self,
        _: &RunId,
    ) -> Result<haider_core::FinalizationGuardDecision, HaiderError> {
        Ok(haider_core::FinalizationGuardDecision::Continue { reminder: None })
    }
}
#[tokio::test]
async fn public_spawn_obeys_parent_finalization_guard_without_provider_work() {
    direct_spawn(false, true).await;
}

struct RejectReadStore(MemoryStore);
#[async_trait]
impl haider_core::StoreHandle for RejectReadStore {
    async fn append(
        &self,
        events: &mut [haider_protocol::envelope::RawEnvelope],
    ) -> Result<haider_core::CommittedRange, HaiderError> {
        self.0.append(events).await
    }
    async fn read(
        &self,
        _: &SessionId,
        _: u64,
        _: usize,
    ) -> Result<Vec<haider_protocol::envelope::RawEnvelope>, HaiderError> {
        Err(HaiderError::new(
            haider_protocol::error::ErrorCode::Internal,
            "injected recovery read failure",
            false,
        ))
    }
    async fn latest_seq(&self, session: &SessionId) -> Result<u64, HaiderError> {
        self.0.latest_seq(session).await
    }
    async fn branch_lineage(
        &self,
        session: &SessionId,
        branch: Option<&haider_protocol::ids::BranchId>,
    ) -> Result<Vec<haider_protocol::branch::BranchDescriptor>, HaiderError> {
        self.0.branch_lineage(session, branch).await
    }
}

#[tokio::test]
async fn public_spawn_recovery_read_failure_retains_child_cancellation_ownership() {
    let session = SessionId::new("failed-recovery-parent");
    let run = RunId::new("failed-recovery-run");
    let mut config = HarnessConfig::for_session(session, DeviceId::new("test"), 0, 2);
    config.agent_spawn = Some(AgentSpawnSpecV1 {
        task: "test".into(),
        prompt: "bounded task".into(),
        model: None,
        provider: None,
        agent_type: None,
        workflow: None,
        workflow_trigger: None,
    });
    let dispatcher = Arc::new(ChildDispatcher {
        calls: AtomicUsize::new(0),
        acknowledgements: AtomicUsize::new(0),
        cancellations: AtomicUsize::new(0),
        red: false,
    });
    let ToolDispatchResult::Deferred(ticket) = dispatcher
        .execute(
            &run,
            &ItemId::new("recovered-tool"),
            "recovered-call",
            "spawn_subagent",
            serde_json::json!({"prompt":"bounded task"}),
            &CancelToken::new(),
        )
        .await
        .expect("ticket")
    else {
        panic!("deferred ticket");
    };
    dispatcher.calls.store(0, Ordering::SeqCst);
    let provider = Arc::new(FakeProvider::new(vec![]));
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        Arc::new(RejectReadStore(MemoryStore::new())),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let result = handle
        .submit_child_wait_turn(haider_core::SubmitChildWaitTurn {
            run_id: run,
            messages: vec![],
            checkpoint: haider_core::ChildWaitCheckpoint {
                tools: vec![haider_core::DeferredToolCheckpoint {
                    ticket,
                    tool_item_id: ItemId::new("recovered-tool"),
                    call_id: "recovered-call".into(),
                    tool_name: "spawn_subagent".into(),
                    args: "{}".into(),
                    report_emitted: false,
                    child_result_emitted: false,
                    tool_result_emitted: false,
                    item_completed: false,
                }],
            },
        })
        .await
        .expect("submit recovery")
        .wait()
        .await
        .expect("recovery outcome");
    assert_eq!(result.state, RunState::Errored);
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 0);
    assert_eq!(dispatcher.cancellations.load(Ordering::SeqCst), 1);
    assert!(provider.requests().is_empty());
    handle.stop().await.expect("stop");
    actor_task.await.expect("actor stopped");
}
