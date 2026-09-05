//! End-to-end actor laws for logical request accounting and durable resume.
// Included from runtime_tests to share its provider/dispatcher fixtures.
use super::*;
use haider_protocol::request_budget::{RequestBudgetPhaseV1, RequestBudgetStatusV1};

fn statuses(events: &[RawEnvelope]) -> Vec<RequestBudgetStatusV1> {
    events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                RequestBudgetStatusV1::from_extension_item(&item)
            }
            _ => None,
        })
        .collect()
}

fn rounds(count: usize) -> Vec<FakeStep> {
    (0..count)
        .flat_map(|round| {
            [
                FakeStep::EmitText {
                    text: format!("partial {round}"),
                },
                FakeStep::EmitToolCall {
                    call_id: format!("budget-tool-{round}"),
                    name: "inspect".into(),
                    args: serde_json::json!({"round":round}),
                },
                FakeStep::Finish {
                    reason: FinishReason::ToolUse,
                },
            ]
        })
        .collect()
}

#[tokio::test]
async fn soft_request_bound_is_once_typed_and_in_the_actual_model_request() {
    let mut bounded = config();
    bounded.provider_request_tranche = 2;
    bounded.max_provider_requests_per_turn = 5;
    let mut script = rounds(4);
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("long work"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    let events = store.events(&SessionId::new(SESSION)).await;
    let states = statuses(&events);
    let soft = states
        .iter()
        .filter(|status| status.phase == RequestBudgetPhaseV1::SoftBound)
        .collect::<Vec<_>>();
    assert_eq!(soft.len(), 1);
    assert_eq!(soft[0].used, 2);
    let note = Message::user_text(soft[0].model_note());
    assert!(!provider.requests()[1].messages.contains(&note));
    for request in &provider.requests()[2..] {
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|message| **message == note)
                .count(),
            1
        );
    }
    let progress = states
        .iter()
        .filter(|status| status.phase == RequestBudgetPhaseV1::Progress)
        .map(|status| status.used)
        .collect::<Vec<_>>();
    assert_eq!(progress, [1, 2, 3, 4, 5]);
    drop(handle);
    task.await.expect("actor joins");
}

#[tokio::test]
async fn hard_request_bound_restores_partial_text_and_tool_history_after_actor_restart() {
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    let store = Arc::new(MemoryStore::new());
    let provider = Arc::new(FakeProvider::new(rounds(3)));
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("keep working"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().expect("named error").code,
        ErrorCode::RequestBudgetExceeded
    );
    let checkpoint: RequestBudgetStatusV1 =
        serde_json::from_value(outcome.error.expect("error").details.expect("details"))
            .expect("typed checkpoint");
    assert_eq!(checkpoint.used, 2);
    assert_eq!(checkpoint.phase, RequestBudgetPhaseV1::HardBound);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 2);
    drop(handle);
    task.await.expect("old actor joins");

    // Reconstruct exclusively from persisted envelopes; no actor messages,
    // provider, or tool dispatcher are reused by the continuation.
    let restored = Arc::new(MemoryStore::new());
    let events = store.events(&checkpoint.continuation.session_id).await;
    restored
        .append_owned(events.clone())
        .await
        .expect("restore journal");
    let reader = GenericArtifactReader {
        artifact: ArtifactRef::new("unused"),
        bytes: Vec::new(),
    };
    let mut messages = PromptHistoryCompiler::compile_idle_with_artifacts(
        restored.as_ref(),
        &reader,
        &checkpoint.continuation.session_id,
        None,
        None,
    )
    .await
    .expect("restore history");
    assert!(messages.iter().any(|message| message.blocks.iter().any(
        |block| matches!(block, Block::Text { text } if text.to_owned_string() == "partial 1")
    )));
    messages.push(Message::user_text("Continue from the checkpoint."));
    let resumed_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::ExpectToolResult {
            call_id: "budget-tool-0".into(),
        },
        FakeStep::ExpectToolResult {
            call_id: "budget-tool-1".into(),
        },
        FakeStep::EmitText {
            text: "finished without repeating tools".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let mut fresh = config();
    fresh.worker_generation += 1;
    let resumed = HarnessActor::spawn(fresh, resumed_provider.clone(), restored);
    let outcome = resumed
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("budget-continuation"),
            messages,
        })
        .await
        .expect("resume accepted")
        .wait()
        .await
        .expect("resumed outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(resumed_provider.requests().len(), 1);
    assert_eq!(
        statuses(&events)
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::HardBound)
            .count(),
        1
    );
}

#[derive(Debug)]
struct ImmediateRetrySleeper;
#[async_trait]
impl haider_core::RetrySleeper for ImmediateRetrySleeper {
    async fn sleep(&self, _delay_ms: u64) {}
}

#[tokio::test]
async fn request_budget_ignores_transport_retries_at_the_soft_and_hard_bounds() {
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    bounded.retry_sleeper = Arc::new(ImmediateRetrySleeper);
    let mut script = vec![FakeStep::Error {
        kind: ProviderErrorKind::Overloaded,
        message: "retry".into(),
        retry_after_ms: None,
    }];
    script.extend(rounds(1));
    script.extend([
        FakeStep::Error {
            kind: ProviderErrorKind::Overloaded,
            message: "retry at hard allowance".into(),
            retry_after_ms: None,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("retry transport"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 4);
    let states = statuses(&store.events(&SessionId::new(SESSION)).await);
    assert_eq!(
        states
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::Progress)
            .map(|status| status.used)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        states
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::SoftBound)
            .count(),
        1
    );
    assert!(
        !states
            .iter()
            .any(|status| status.phase == RequestBudgetPhaseV1::HardBound)
    );
    drop(handle);
    task.await.expect("actor joins");
}

#[test]
fn default_request_budget_covers_the_reported_fifty_three_round_workload() {
    assert_eq!(config().provider_request_tranche, 32);
    assert_eq!(config().max_provider_requests_per_turn, 64);
}

#[tokio::test]
async fn default_budget_completes_fifty_three_logical_requests() {
    let mut script = rounds(52);
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("complete fifty-three rounds"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 53);
    assert_eq!(
        statuses(&store.events(&SessionId::new(SESSION)).await)
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::SoftBound)
            .count(),
        1
    );
    drop(handle);
    task.await.expect("actor joins");
}

#[tokio::test]
async fn recovered_child_checkpoint_restores_budget_even_without_legacy_count() {
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 4;
    let provider = Arc::new(FakeProvider::new(rounds(5)));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded.clone(),
        provider,
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("durable work"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    let status: RequestBudgetStatusV1 =
        serde_json::from_value(outcome.error.expect("cap error").details.expect("details"))
            .expect("status");
    drop(handle);
    task.await.expect("old actor joins");
    let mut journal = store.events(&SessionId::new(SESSION)).await;
    // Snapshot the durable boundary after two settled tool rounds and before
    // the third provider dispatch. Legacy child/menu recovery supplies no
    // logical count; the typed progress journal must restore it.
    let settled = journal.iter().position(|event| matches!(typed(event), EventPayload::Item(ItemEvent::Completed { item: TurnItem::ToolCall { ref call_id, .. }, .. }) if call_id == "budget-tool-1")).expect("second settled tool");
    let boundary = settled
        + journal[settled..]
            .iter()
            .position(|event| matches!(typed(event), EventPayload::NodeCommitted(_)))
            .expect("atomic settled tool node")
        + 1;
    journal.truncate(boundary);
    let restored = Arc::new(MemoryStore::new());
    restored
        .append_owned(journal)
        .await
        .expect("restore prefix");
    bounded.worker_generation += 1;
    assert_eq!(bounded.provider_requests_already_made, 0);
    assert_eq!(bounded.provider_request_ordinal_already_made, 0);
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let (recovered_actor, actor) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        restored.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let recovered_task = tokio::spawn(recovered_actor.run());
    let outcome = actor
        .submit_child_wait_turn(haider_core::SubmitChildWaitTurn {
            run_id: status.continuation.run_id,
            messages: vec![Message::user_text("durable work")],
            checkpoint: haider_core::ChildWaitCheckpoint { tools: Vec::new() },
        })
        .await
        .expect("recover accepted")
        .wait()
        .await
        .expect("recover outcome");
    assert_eq!(outcome.state, RunState::Done);
    let states = statuses(&restored.events(&SessionId::new(SESSION)).await);
    assert_eq!(
        states
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::SoftBound)
            .count(),
        1
    );
    assert_eq!(
        states
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::Progress)
            .map(|status| status.used)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(provider.requests()[0].messages.iter().any(|message| message.blocks.iter().any(|block| matches!(block, Block::Text { text } if text.to_owned_string().contains("[provider_request_budget_v1]")))));
    drop(actor);
    recovered_task.await.expect("recovered actor joins");
}
