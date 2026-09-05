//! End-to-end actor laws for logical request accounting and durable resume.
// Included from runtime_tests to share its provider/dispatcher fixtures.
use super::*;
use haider_protocol::ceiling::{INTERNAL_CEILING_EXIT_CODE, InternalCeilingTerminalV1};
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

#[derive(Debug)]
struct RebindUnavailableAfterBudget {
    refreshes: AtomicUsize,
    hard_cap: usize,
}

#[async_trait]
impl ProviderRebindResolver for RebindUnavailableAfterBudget {
    async fn refresh(
        &self,
        _model: &str,
        _reasoning: &str,
    ) -> Result<Option<ProviderRebindTarget>, HaiderError> {
        if self.refreshes.fetch_add(1, Ordering::SeqCst) >= self.hard_cap {
            return Err(HaiderError::new(
                ErrorCode::ProviderError,
                "rebound provider is no longer available",
                false,
            ));
        }
        Ok(None)
    }
}

/// MUTATION CHECK: refresh the provider before checking the exhausted logical
/// budget. A newly unavailable route would replace the durable cap and receipts
/// even though no further provider request is allowed.
#[tokio::test]
async fn hard_request_bound_preserves_typed_terminal_before_provider_rebind_refresh() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("existing.txt"), "original").expect("baseline file");
    let resolver = Arc::new(RebindUnavailableAfterBudget {
        refreshes: AtomicUsize::new(0),
        hard_cap: 2,
    });
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    bounded.ceiling_workspace = Some(workspace.path().into());
    bounded.provider_rebind_resolver = Some(resolver.clone());
    let provider = Arc::new(FakeProvider::new(rounds(3)));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("inspect until the cap"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("typed cap cause");
    assert_eq!(error.code, ErrorCode::RequestBudgetExceeded);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(resolver.refreshes.load(Ordering::SeqCst), 2);
    let events = store.events(&SessionId::new(SESSION)).await;
    let terminals = events
        .iter()
        .filter(|event| InternalCeilingTerminalV1::from_payload(&event.payload).is_some())
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    let terminal = &terminals[0].payload["terminal"];
    assert_eq!(terminal["exit_code"], INTERNAL_CEILING_EXIT_CODE);
    assert_eq!(terminal["workspace_state"], "untouched");
    assert_eq!(terminal["workspace_before"], terminal["workspace_after"]);
    assert_eq!(terminal["partial_progress"]["tool_calls"], 2);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 2);
    assert_eq!(error.details.expect("cap details")["terminal"], *terminal);
    assert_eq!(statuses(&events).last().expect("hard checkpoint").used, 2);
    drop(handle);
    task.await.expect("actor joins");
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

/// MUTATION CHECK: omit the terminal block, miscount request/tool progress, or
/// render the private pre-turn receipt into provider history. The actual cap
/// and replayed prompt assertions independently expose each regression.
#[tokio::test]
async fn capped_actor_seals_untouched_tree_and_exact_partial_progress_with_hidden_baseline() {
    let workspace = tempfile::tempdir().expect("workspace");
    let private_name = "ceiling-private-preexisting-dirt.txt";
    std::fs::write(
        workspace.path().join(private_name),
        "existing uncommitted content",
    )
    .expect("preexisting dirty workspace");
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    bounded.ceiling_workspace = Some(workspace.path().into());
    let provider = Arc::new(FakeProvider::new(rounds(3)));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("continue inspecting"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("typed cap cause");
    assert_eq!(error.code, ErrorCode::RequestBudgetExceeded);
    assert_eq!(provider.requests().len(), 2);
    let events = store.events(&SessionId::new(SESSION)).await;
    let terminal_events = events
        .iter()
        .filter_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| (event, event.payload["terminal"].clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_events.len(), 1);
    let (terminal_event, terminal) = &terminal_events[0];
    assert_eq!(terminal["end_reason"], "harness_internal_ceiling");
    assert_eq!(terminal["internal_cap_detected"], true);
    assert_eq!(terminal["exit_code"], INTERNAL_CEILING_EXIT_CODE);
    assert_eq!(
        terminal["ceilings"],
        serde_json::json!({"soft":1,"hard":2,"used":2})
    );
    assert_eq!(terminal["continuation"]["session_id"], SESSION);
    assert_eq!(
        terminal_event.run_id.as_ref().map(RunId::as_str),
        terminal["continuation"]["run_id"].as_str()
    );
    assert_eq!(terminal["workspace_state"], "untouched");
    assert_eq!(terminal["workspace_before"], terminal["workspace_after"]);
    assert_eq!(
        terminal["partial_progress"]["files_written"],
        serde_json::json!([])
    );
    assert_eq!(
        terminal["partial_progress"]["files_deleted"],
        serde_json::json!([])
    );
    assert_eq!(terminal["partial_progress"]["tool_calls"], 2);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 2);
    assert_eq!(error.details.expect("cap details")["terminal"], *terminal);

    let baseline_events = events
        .iter()
        .filter(|event| {
            event
                .payload
                .pointer("/item/kind")
                .and_then(serde_json::Value::as_str)
                == Some("turn_workspace_before_v1")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        baseline_events.len(),
        2,
        "one durable started/completed baseline pair"
    );
    for baseline in &baseline_events {
        assert!(!baseline.render.ui);
        assert_eq!(baseline.render.prompt, PromptRender::Omit);
        assert!(baseline.render.durable);
    }
    let first_request_fact = events
        .iter()
        .find(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                RequestBudgetStatusV1::from_extension_item(&item).is_some_and(|status| {
                    status.phase == RequestBudgetPhaseV1::Progress && status.used == 1
                })
            }
            _ => false,
        })
        .expect("first request durable marker");
    assert!(baseline_events[1].seq < first_request_fact.seq);
    for request in provider.requests() {
        let messages = serde_json::to_string(&request.messages).expect("provider messages");
        assert!(!messages.contains("turn_workspace_before_v1"));
        assert!(!messages.contains(private_name));
    }
    let reader = GenericArtifactReader {
        artifact: ArtifactRef::new("unused"),
        bytes: Vec::new(),
    };
    let messages = PromptHistoryCompiler::compile_idle_with_artifacts(
        store.as_ref(),
        &reader,
        &SessionId::new(SESSION),
        None,
        None,
    )
    .await
    .expect("compile durable history");
    let messages = serde_json::to_string(&messages).expect("compiled messages");
    assert!(!messages.contains("turn_workspace_before_v1"));
    assert!(!messages.contains(private_name));
    drop(handle);
    task.await.expect("actor joins");
}

/// MUTATION CHECK: capture a new baseline during same-run recovery. The only
/// filesystem edits happen between the retained first-request prefix and the
/// restarted actor, so recapturing would falsely report an untouched workspace.
#[tokio::test]
async fn capped_actor_recovery_uses_original_durable_tree_receipt_and_prior_tool_progress() {
    let workspace = tempfile::tempdir().expect("workspace");
    // Two hundred path/digest entries exceed one 16 KiB receipt chunk. The
    // receipt remains before provider dispatch, while every envelope stays
    // comfortably below the negotiated transport frame size.
    for index in 0..200 {
        std::fs::write(
            workspace
                .path()
                .join(format!("receipt-padding-{index:04}.txt")),
            "",
        )
        .expect("receipt padding file");
    }
    std::fs::write(workspace.path().join("edited.txt"), "before").expect("existing file");
    std::fs::write(workspace.path().join("deleted.txt"), "remove").expect("deleted original");
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    bounded.ceiling_workspace = Some(workspace.path().into());
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded.clone(),
        Arc::new(FakeProvider::new(rounds(3))),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("durable inspection"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    drop(handle);
    task.await.expect("original actor joins");
    let mut journal = store.events(&SessionId::new(SESSION)).await;
    let original_terminal = journal
        .iter()
        .find_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| event.payload["terminal"].clone())
        })
        .expect("original cap terminal");
    assert_eq!(original_terminal["workspace_state"], "untouched");
    let run_id = RunId::new(
        original_terminal["continuation"]["run_id"]
            .as_str()
            .expect("source run id"),
    );
    let first_settled = journal.iter().position(|event| matches!(typed(event),
        EventPayload::Item(ItemEvent::Completed { item: TurnItem::ToolCall { ref call_id, .. }, .. })
            if call_id == "budget-tool-0"
    )).expect("first settled tool");
    // A tool may settle while its provider response is still streaming. The
    // soft-bound note is written after that entire response closes and before
    // the second request dispatch, so this is a recoverable request boundary.
    let boundary = journal
        .iter()
        .position(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed { item, .. }) => {
                RequestBudgetStatusV1::from_extension_item(&item).is_some_and(|status| {
                    status.phase == RequestBudgetPhaseV1::SoftBound && status.used == 1
                })
            }
            _ => false,
        })
        .expect("soft note after completed first response")
        + 1;
    assert!(first_settled < boundary);
    journal.truncate(boundary);
    assert_eq!(
        statuses(&journal)
            .iter()
            .filter(|status| status.phase == RequestBudgetPhaseV1::Progress)
            .map(|status| status.used)
            .collect::<Vec<_>>(),
        [1]
    );
    let original_receipt = journal
        .iter()
        .filter(|event| {
            event
                .payload
                .get("event")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
                && event
                    .payload
                    .pointer("/item/kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("turn_workspace_before_v1")
        })
        .map(|event| event.payload["item"]["data"].clone())
        .collect::<Vec<_>>();
    assert!(
        original_receipt.len() > 1,
        "large baseline has multiple completed chunks"
    );
    for chunk in &original_receipt {
        assert!(serde_json::to_vec(chunk).expect("encoded chunk").len() < 24 * 1024);
    }
    let restored = Arc::new(MemoryStore::new());
    restored
        .append_owned(journal)
        .await
        .expect("restore first-request prefix");
    std::fs::write(workspace.path().join("edited.txt"), "after!")
        .expect("same-size edit during restart");
    std::fs::write(workspace.path().join("created.txt"), "new").expect("create during restart");
    std::fs::remove_file(workspace.path().join("deleted.txt")).expect("delete during restart");
    let reader = GenericArtifactReader {
        artifact: ArtifactRef::new("unused"),
        bytes: Vec::new(),
    };
    // The current-run compiler deliberately emits only its accepted user;
    // recovery supplies already-settled response checkpoints separately.
    let mut messages = PromptHistoryCompiler::compile_with_artifacts(
        restored.as_ref(),
        &reader,
        &SessionId::new(SESSION),
        None,
        None,
        &run_id,
    )
    .await
    .expect("compile restored messages");
    let restored_events = restored.events(&SessionId::new(SESSION)).await;
    let (call_id, name, args) = restored_events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed {
                item:
                    TurnItem::ToolCall {
                        call_id,
                        name,
                        args,
                        status: ToolStatus::Completed,
                    },
                ..
            }) if call_id == "budget-tool-0" => Some((call_id, name, args)),
            _ => None,
        })
        .expect("durable settled call checkpoint");
    let result = restored_events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::ToolResult {
                call_id: result_call_id,
                result,
            } if result_call_id == call_id => Some(result),
            _ => None,
        })
        .expect("durable settled result checkpoint");
    messages.push(Message::assistant(vec![Block::ToolCall {
        call_id: call_id.clone(),
        name,
        args,
    }]));
    messages.push(Message::tool_result_with_images(
        call_id,
        result.preview,
        result.truncated,
        result.images,
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.tool_result_for("budget-tool-0").is_some()),
        "settled checkpoint must reach the resumed provider"
    );
    bounded.worker_generation += 1;
    bounded.provider_requests_already_made = 1;
    bounded.provider_request_ordinal_already_made = 1;
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::ExpectToolResult {
            call_id: "budget-tool-0".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "budget-tool-1".into(),
            name: "inspect".into(),
            args: serde_json::json!({"round":1}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let (recovered_actor, recovered) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        restored.clone(),
        Some(dispatcher.clone()),
    );
    let recovered_task = tokio::spawn(recovered_actor.run());
    let outcome = recovered
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: run_id.clone(),
            messages,
        })
        .await
        .expect("recover accept")
        .wait()
        .await
        .expect("recovered outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().expect("cap cause").code,
        ErrorCode::RequestBudgetExceeded,
        "recovered failure: {:?}",
        outcome.error
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(
        dispatcher.calls.load(Ordering::SeqCst),
        1,
        "settled first tool is not repeated"
    );
    let recovered_journal = restored.events(&SessionId::new(SESSION)).await;
    let terminal = recovered_journal
        .iter()
        .find_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| event.payload["terminal"].clone())
        })
        .expect("recovered cap terminal");
    assert_eq!(terminal["end_reason"], "harness_internal_ceiling");
    assert_eq!(terminal["continuation"]["run_id"], run_id.as_str());
    assert_eq!(
        terminal["ceilings"],
        serde_json::json!({"soft":1,"hard":2,"used":2})
    );
    assert_eq!(terminal["workspace_state"], "mutated");
    assert_eq!(
        terminal["workspace_before"],
        original_terminal["workspace_before"]
    );
    assert_ne!(terminal["workspace_before"], terminal["workspace_after"]);
    assert_eq!(
        terminal["partial_progress"]["files_written"],
        serde_json::json!(["created.txt", "edited.txt"])
    );
    assert_eq!(
        terminal["partial_progress"]["files_deleted"],
        serde_json::json!(["deleted.txt"])
    );
    assert_eq!(terminal["partial_progress"]["tool_calls"], 2);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 2);
    let recovered_receipt = recovered_journal
        .iter()
        .filter(|event| {
            event
                .payload
                .get("event")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
                && event
                    .payload
                    .pointer("/item/kind")
                    .and_then(serde_json::Value::as_str)
                    == Some("turn_workspace_before_v1")
        })
        .map(|event| event.payload["item"]["data"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_receipt, original_receipt,
        "recovery retains exactly the original baseline"
    );
    let messages =
        serde_json::to_string(&provider.requests()[0].messages).expect("resumed messages");
    assert!(!messages.contains("turn_workspace_before_v1"));
    assert!(!messages.contains("edited.txt"));
    drop(recovered);
    recovered_task.await.expect("recovered actor joins");
}

/// MUTATION CHECK: deduplicate the progress count by provider call-id. A
/// provider may reuse the same ID in another request; two durable completed
/// tool results still represent two calls made by this capped run.
#[tokio::test]
async fn capped_actor_counts_reused_provider_call_id_in_each_logical_request() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 2;
    bounded.ceiling_workspace = Some(workspace.path().into());
    let mut script = rounds(2);
    for step in &mut script {
        if let FakeStep::EmitToolCall { call_id, .. } = step {
            *call_id = "reused-call-id".into();
        }
    }
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
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
        .submit_turn(SubmitTurn::new("repeat inspections"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("cap error").code,
        ErrorCode::RequestBudgetExceeded
    );
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 2);
    let journal = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(
        journal
            .iter()
            .filter(|event| {
                event
                    .payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    == Some("tool_result")
                    && event
                        .payload
                        .get("call_id")
                        .and_then(serde_json::Value::as_str)
                        == Some("reused-call-id")
            })
            .count(),
        2
    );
    let terminal = journal
        .iter()
        .find_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| event.payload["terminal"].clone())
        })
        .expect("typed cap terminal");
    assert_eq!(terminal["partial_progress"]["tool_calls"], 2);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 2);
    drop(handle);
    task.await.expect("actor joins");
}

struct RemoveCeilingWorkspaceDispatcher {
    root: std::path::PathBuf,
}

#[async_trait]
impl ToolDispatcher for RemoveCeilingWorkspaceDispatcher {
    async fn execute(
        &self,
        run_id: &RunId,
        item_id: &ItemId,
        call_id: &str,
        name: &str,
        args: serde_json::Value,
        cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        let result = CompletingDispatcher
            .execute(run_id, item_id, call_id, name, args, cancel)
            .await?;
        std::fs::remove_dir_all(&self.root).expect("remove workspace after durable request");
        Ok(result)
    }
}

/// MUTATION CHECK: propagate a post-receipt failure as generic software error,
/// invent untouched from missing evidence, or discard partial work. The cap
/// cause/exit and tool progress must survive an unreadable after-tree.
#[tokio::test]
async fn capped_actor_preserves_typed_cap_and_progress_when_post_tree_is_unavailable() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut bounded = config();
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 1;
    bounded.ceiling_workspace = Some(workspace.path().into());
    let provider = Arc::new(FakeProvider::new(rounds(1)));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider,
        store.clone(),
        Some(Arc::new(RemoveCeilingWorkspaceDispatcher {
            root: workspace.path().into(),
        })),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("remove workspace"))
        .await
        .expect("accept")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("cap error").code,
        ErrorCode::RequestBudgetExceeded
    );
    let journal = store.events(&SessionId::new(SESSION)).await;
    let terminal = journal
        .iter()
        .find_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| event.payload["terminal"].clone())
        })
        .expect("typed cap terminal despite missing workspace");
    assert_eq!(terminal["end_reason"], "harness_internal_ceiling");
    assert_eq!(terminal["internal_cap_detected"], true);
    assert_eq!(terminal["exit_code"], INTERNAL_CEILING_EXIT_CODE);
    assert_eq!(
        terminal["ceilings"],
        serde_json::json!({"soft":1,"hard":1,"used":1})
    );
    assert_eq!(terminal["workspace_receipt_error"]["phase"], "after");
    assert!(terminal.get("workspace_state").is_none());
    assert!(terminal.get("workspace_after").is_none());
    assert!(terminal["workspace_before"].as_str().is_some());
    assert!(terminal["partial_progress"].get("files_written").is_none());
    assert!(terminal["partial_progress"].get("files_deleted").is_none());
    assert_eq!(terminal["partial_progress"]["tool_calls"], 1);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 1);
    assert_eq!(terminal["continuation"]["session_id"], SESSION);
    drop(handle);
    task.await.expect("actor joins");
}

/// MUTATION CHECK: reject ordinary chat when the baseline cannot be captured,
/// or turn a later genuine cap into a software failure. An unavailable
/// workspace is receipt evidence, not permission to invent an untouched tree.
#[tokio::test]
async fn unavailable_pre_turn_tree_allows_chat_and_preserves_cap_with_partial_progress() {
    let workspace = tempfile::tempdir().expect("workspace parent");
    let missing = workspace.path().join("missing-workspace");
    let mut bounded = config();
    bounded.ceiling_workspace = Some(missing);
    bounded.provider_request_tranche = 1;
    bounded.max_provider_requests_per_turn = 1;
    let chat_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "chat still works".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let chat_store = Arc::new(MemoryStore::new());
    let (chat_actor, chat) = HarnessActor::new_with_dispatcher(
        bounded.clone(),
        chat_provider.clone(),
        chat_store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let chat_task = tokio::spawn(chat_actor.run());
    let chat_outcome = chat
        .submit_turn(SubmitTurn::new("plain chat"))
        .await
        .expect("chat accepted")
        .wait()
        .await
        .expect("chat outcome");
    assert_eq!(chat_outcome.state, RunState::Done);
    assert!(chat_outcome.error.is_none());
    assert_eq!(chat_provider.requests().len(), 1);
    let chat_journal = chat_store.events(&SessionId::new(SESSION)).await;
    assert!(chat_journal.iter().any(|event| matches!(typed(event),
        EventPayload::Item(ItemEvent::Completed { item: TurnItem::AgentMessage { ref text }, .. })
            if text.to_owned_string() == "chat still works"
    )));
    assert!(
        !chat_journal
            .iter()
            .any(|event| InternalCeilingTerminalV1::from_payload(&event.payload).is_some())
    );
    drop(chat);
    chat_task.await.expect("chat actor joins");

    let provider = Arc::new(FakeProvider::new(rounds(2)));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("continue inspecting"))
        .await
        .expect("cap run accepted")
        .wait()
        .await
        .expect("cap outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("typed cap cause").code,
        ErrorCode::RequestBudgetExceeded
    );
    assert_eq!(provider.requests().len(), 1);
    let journal = store.events(&SessionId::new(SESSION)).await;
    let terminal = journal
        .iter()
        .find_map(|event| {
            InternalCeilingTerminalV1::from_payload(&event.payload)
                .map(|_| event.payload["terminal"].clone())
        })
        .expect("typed cap terminal despite missing baseline");
    assert_eq!(terminal["end_reason"], "harness_internal_ceiling");
    assert_eq!(terminal["internal_cap_detected"], true);
    assert_eq!(terminal["exit_code"], INTERNAL_CEILING_EXIT_CODE);
    assert_eq!(
        terminal["ceilings"],
        serde_json::json!({"soft":1,"hard":1,"used":1})
    );
    assert_eq!(terminal["workspace_receipt_error"]["phase"], "before");
    assert!(terminal.get("workspace_state").is_none());
    assert!(terminal.get("workspace_before").is_none());
    assert!(terminal.get("workspace_after").is_none());
    assert!(terminal["partial_progress"].get("files_written").is_none());
    assert!(terminal["partial_progress"].get("files_deleted").is_none());
    assert_eq!(terminal["partial_progress"]["tool_calls"], 1);
    assert_eq!(terminal["partial_progress"]["last_request_ordinal"], 1);
    assert_eq!(terminal["continuation"]["session_id"], SESSION);
    drop(handle);
    task.await.expect("capped actor joins");
}
