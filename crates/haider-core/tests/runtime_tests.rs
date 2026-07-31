#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CommittedRange, ContextCompactor, HarnessActor, HarnessConfig, HarnessHandle, MemoryStore,
    ProviderAttemptDecision, ProviderAttemptResolver, ResolvedProviderAttempt, StoreHandle,
    SubmitCommittedTurn, SubmitTurn, ToolDispatchResult, ToolDispatcher,
};
use haider_protocol::EventPayload;
use haider_protocol::credential::{RotationCause, RotationEvent};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CONTINUATION_CHECKPOINT_EXTENSION_KIND, CompactionIntent,
    CompactionResume, ContinuationCheckpoint,
};
use haider_protocol::ids::{CredentialAlias, DeviceId, ItemId, NodeId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason, Usage, UsageSource};
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_provider::{
    FakeProvider, FakeStep, Message, Provider, ProviderError, ProviderErrorKind, ProviderStream,
    TurnRequest,
};
use std::collections::HashSet;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, advance, sleep, timeout};

const SESSION: &str = "session-test";

fn config() -> HarnessConfig {
    HarnessConfig::for_session(
        SessionId::new(SESSION),
        DeviceId::new("device-test"),
        17,
        23,
    )
}

fn runtime(script: Vec<FakeStep>) -> (HarnessHandle, Arc<MemoryStore>, Arc<FakeProvider>) {
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    (handle, store, provider)
}

fn typed(envelope: &RawEnvelope) -> EventPayload {
    serde_json::from_value(envelope.payload.clone()).expect("known payload")
}

fn completed_extension(envelope: &RawEnvelope, expected_kind: &str) -> bool {
    matches!(
        typed(envelope),
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::Extension { kind, .. },
            ..
        }) if kind == expected_kind
    )
}

fn normalize(mut payload: serde_json::Value) -> serde_json::Value {
    if payload["type"] == "item" {
        payload["item_id"] = serde_json::Value::String("<item>".into());
    }
    if payload["type"] == "node_committed" {
        payload["node"] = serde_json::Value::String("<node>".into());
        if payload.get("parent").is_some() {
            payload["parent"] = serde_json::Value::String("<node>".into());
        }
    }
    payload
}

fn assert_items_closed_before_terminal(events: &[RawEnvelope]) {
    let mut open = HashSet::<ItemId>::new();
    let mut saw_terminal = false;
    for event in events {
        assert!(!saw_terminal, "an envelope followed the terminal run state");
        match typed(event) {
            EventPayload::Item(ItemEvent::Started { item_id, .. }) => {
                assert!(open.insert(item_id), "an item started twice");
            }
            EventPayload::Item(ItemEvent::Completed { item_id, .. }) => {
                assert!(open.remove(&item_id), "completed item was not open");
            }
            EventPayload::RunState(state) if state.is_terminal() => {
                assert!(open.is_empty(), "terminal run state left open items");
                saw_terminal = true;
            }
            _ => {}
        }
    }
    assert!(saw_terminal, "fixture did not commit a terminal run state");
    assert!(open.is_empty(), "fixture ended with open items");
}

#[tokio::test]
async fn full_turn_commits_exact_projected_sequence() {
    let usage = Usage {
        input: 11,
        output: 7,
        reasoning: 2,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
    };
    let (handle, store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "hello".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "call-1".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
        FakeStep::EmitUsage {
            usage: usage.clone(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("do the work"))
        .await
        .expect("turn accepted");
    let outcome = turn.wait().await.expect("actor reports outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.finish_reason, FinishReason::ToolUse);

    let events = store.events(&SessionId::new(SESSION)).await;
    let actual: Vec<_> = events
        .iter()
        .map(|event| normalize(event.payload.clone()))
        .collect();
    let expected = vec![
        serde_json::json!({"type":"run_state","state":"queued"}),
        serde_json::json!({
            "type":"user_message",
            "text":"do the work",
            "attachments":[],
            "mode":"steer"
        }),
        serde_json::json!({
            "type":"node_committed",
            "node":"<node>",
            "kind":{
                "kind":"user_turn",
                "text":"do the work",
                "attachments":[]
            }
        }),
        serde_json::json!({"type":"run_state","state":"thinking"}),
        serde_json::json!({"type":"run_state","state":"streaming"}),
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{"item":"agent_message","text":""}
        }),
        serde_json::json!({
            "type":"item",
            "event":"delta",
            "item_id":"<item>",
            "delta":{"delta":"text","text":"hello"}
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{"item":"agent_message","text":"hello"}
        }),
        serde_json::json!({
            "type":"node_committed",
            "node":"<node>",
            "parent":"<node>",
            "kind":{
                "kind":"assistant_commit",
                "text":"hello",
                "verdict":{"verdict":"not_applicable"}
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"tool_call",
                "call_id":"call-1",
                "name":"inspect",
                "args":{},
                "status":"in_progress"
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"delta",
            "item_id":"<item>",
            "delta":{
                "delta":"tool_args",
                "fragment":"{\"path\":\"src/lib.rs\"}"
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"tool_call",
                "call_id":"call-1",
                "name":"inspect",
                "args":{"path":"src/lib.rs"},
                "status":"pending"
            }
        }),
        serde_json::json!({
            "type":"node_committed",
            "node":"<node>",
            "parent":"<node>",
            "kind":{
                "kind":"tool_exchange",
                "tool":"inspect",
                "summary":"tool call settled as Pending"
            }
        }),
        serde_json::to_value(EventPayload::Usage(usage)).expect("usage serializes"),
        serde_json::json!({"type":"run_state","state":"done"}),
    ];
    assert_eq!(actual, expected);
    assert!(events.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
    assert!(
        events
            .iter()
            .all(|event| event.authority_epoch == 17 && event.worker_generation == 23)
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "fake-model");
    assert_eq!(requests[0].max_tokens, 4096);
}

/// MUTATION CHECK: treat MaxTokens as terminal or omit the hidden continue
/// instruction. Expected runtime failure: only one request is observed, the
/// partial assistant block/instruction is absent, or no durable seam exists.
#[tokio::test]
async fn max_tokens_continues_in_the_same_logical_run() {
    let (handle, store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "partial answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::EmitText {
            text: " completed".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);

    let outcome = handle
        .submit_turn(SubmitTurn::new("write a long answer"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.finish_reason, FinishReason::EndTurn);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second_text = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(second_text.contains(&"partial answer"));
    assert!(
        second_text
            .contains(&"Continue exactly where you stopped. Do not repeat completed content.")
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                typed(event),
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { ref kind, .. },
                    ..
                }) if kind == CONTINUATION_CHECKPOINT_EXTENSION_KIND
            ))
            .count(),
        1
    );
    let checkpoint = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            }) if kind == CONTINUATION_CHECKPOINT_EXTENSION_KIND => {
                serde_json::from_value::<ContinuationCheckpoint>(data).ok()
            }
            _ => None,
        })
        .expect("typed continuation checkpoint");
    assert_eq!(checkpoint.reason, FinishReason::MaxTokens);
    assert_eq!(checkpoint.request_index, 1);
}

/// MUTATION CHECK: remove the continuation cap. Expected runtime failure:
/// this scripted provider reaches a fourth request or the run ends Done.
#[tokio::test]
async fn repeated_max_tokens_is_bounded_independently() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let mut bounded = config();
    bounded.max_continuations_per_turn = 2;
    let handle = HarnessActor::spawn(bounded, provider.clone(), store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("never-ending output"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("bounded error").code,
        ErrorCode::LoopLimit
    );
    assert_eq!(provider.requests().len(), 3);
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| completed_extension(event, CONTINUATION_CHECKPOINT_EXTENSION_KIND))
            .count(),
        2
    );
}

#[derive(Debug, Default)]
struct FakeContextCompactor {
    calls: AtomicUsize,
}

#[async_trait]
impl ContextCompactor for FakeContextCompactor {
    async fn plan(
        &self,
        _run_id: &RunId,
        resume_cause: CompactionResume,
    ) -> Result<CompactionIntent, HaiderError> {
        Ok(CompactionIntent {
            operation_id: "forced-test-compaction".into(),
            covers_from: NodeId::new("old-root"),
            covers_to: NodeId::new("old-head"),
            resume_cause,
        })
    }

    async fn compact(
        &self,
        _run_id: &RunId,
        _intent: &CompactionIntent,
        covered_messages: Vec<Message>,
    ) -> Result<Message, HaiderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(covered_messages, [Message::user_text("old history")]);
        Ok(Message::user_text("compacted history"))
    }
}

#[derive(Debug, Default)]
struct ShrinkingContextCompactor {
    calls: AtomicUsize,
}

#[async_trait]
impl ContextCompactor for ShrinkingContextCompactor {
    async fn plan(
        &self,
        _run_id: &RunId,
        resume_cause: CompactionResume,
    ) -> Result<CompactionIntent, HaiderError> {
        Ok(CompactionIntent {
            operation_id: "hard-fit-test-compaction".into(),
            covers_from: NodeId::new("old-root"),
            covers_to: NodeId::new("old-head"),
            resume_cause,
        })
    }

    async fn compact(
        &self,
        _run_id: &RunId,
        _intent: &CompactionIntent,
        covered_messages: Vec<Message>,
    ) -> Result<Message, HaiderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(covered_messages.len(), 1);
        Ok(Message::user_text("s"))
    }
}

fn estimated_input_tokens(config: &HarnessConfig, messages: &[Message]) -> u64 {
    let bytes = serde_json::to_vec(&(
        messages,
        &config.system_prompt,
        &config.tools,
        &config.attachments,
    ))
    .expect("estimate serializes")
    .len();
    u64::try_from(bytes).unwrap_or(u64::MAX).saturating_add(3) / 4
}

/// MUTATION CHECK: route ContextExceeded through generic retry or omit the
/// one-shot guard. Expected runtime failure: no CompactionIntent is durable,
/// the retry lacks the summary, or the double-overflow case makes >2 calls.
#[tokio::test]
async fn context_overflow_forces_one_compaction_and_only_one_retry() {
    let compactor = Arc::new(FakeContextCompactor::default());
    let success_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::ContextExceeded,
            message: "too much context".into(),
            retry_after_ms: None,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let success_store = Arc::new(MemoryStore::new());
    let mut success_config = config();
    success_config.context_compactor = Some(compactor.clone());
    let success = HarnessActor::spawn(
        success_config,
        success_provider.clone(),
        success_store.clone(),
    );
    let outcome = success
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("forced-success"),
            messages: vec![
                Message::user_text("old history"),
                Message::user_text("current"),
            ],
        })
        .await
        .expect("accepted forced-compaction run")
        .wait()
        .await
        .expect("forced-compaction outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(success_provider.requests().len(), 2);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert!(
        success_provider.requests()[1]
            .messages
            .contains(&Message::user_text("compacted history"))
    );
    assert!(
        success_store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .any(|event| completed_extension(event, COMPACTION_INTENT_EXTENSION_KIND))
    );

    let double_compactor = Arc::new(FakeContextCompactor::default());
    let double_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::ContextExceeded,
            message: "first overflow".into(),
            retry_after_ms: None,
        },
        FakeStep::Error {
            kind: ProviderErrorKind::ContextExceeded,
            message: "second overflow".into(),
            retry_after_ms: None,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let mut double_config = config();
    double_config.context_compactor = Some(double_compactor.clone());
    let double = HarnessActor::spawn(
        double_config,
        double_provider.clone(),
        Arc::new(MemoryStore::new()),
    );
    let outcome = double
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("forced-double"),
            messages: vec![
                Message::user_text("old history"),
                Message::user_text("current"),
            ],
        })
        .await
        .expect("accepted double-overflow run")
        .wait()
        .await
        .expect("double-overflow outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("typed repeated overflow");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.message.contains("ContextExceeded"));
    assert_eq!(double_provider.requests().len(), 2);
    assert_eq!(double_compactor.calls.load(Ordering::Relaxed), 1);
}

/// MUTATION CHECK: omit the pre-request hard-fit check or treat an unknown
/// window as guessed. Expected runtime failure: the known-window request
/// still contains the oversized prefix, or the unknown-window case compacts.
#[tokio::test]
async fn known_window_hard_fit_compacts_while_unknown_window_stays_disabled() {
    let long_history = "old ".repeat(256);
    let current = Message::user_text("current");

    let known_compactor = Arc::new(ShrinkingContextCompactor::default());
    let known_provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let mut known_config = config();
    known_config.context_window = Some(128);
    known_config.reserved_output_tokens = 16;
    known_config.context_compactor = Some(known_compactor.clone());
    let known = HarnessActor::spawn(
        known_config,
        known_provider.clone(),
        Arc::new(MemoryStore::new()),
    );
    let outcome = known
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("known-hard-fit"),
            messages: vec![Message::user_text(long_history.clone()), current.clone()],
        })
        .await
        .expect("known-window turn accepted")
        .wait()
        .await
        .expect("known-window outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(known_compactor.calls.load(Ordering::Relaxed), 1);
    assert_eq!(known_provider.requests().len(), 1);
    assert!(
        !known_provider.requests()[0]
            .messages
            .contains(&Message::user_text(long_history.clone()))
    );

    let unknown_compactor = Arc::new(ShrinkingContextCompactor::default());
    let unknown_provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let mut unknown_config = config();
    unknown_config.context_window = None;
    unknown_config.reserved_output_tokens = u64::MAX;
    unknown_config.context_compactor = Some(unknown_compactor.clone());
    let unknown = HarnessActor::spawn(
        unknown_config,
        unknown_provider.clone(),
        Arc::new(MemoryStore::new()),
    );
    let outcome = unknown
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("unknown-hard-fit"),
            messages: vec![Message::user_text(long_history.clone()), current],
        })
        .await
        .expect("unknown-window turn accepted")
        .wait()
        .await
        .expect("unknown-window outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(unknown_compactor.calls.load(Ordering::Relaxed), 0);
    assert!(
        unknown_provider.requests()[0]
            .messages
            .contains(&Message::user_text(long_history))
    );
}

/// MUTATION CHECK: check hard-fit only at logical-turn start. Expected
/// runtime failure: the second request carries the un-compacted prefix after
/// MaxTokens grows the same-run continuation past the known input budget.
#[tokio::test]
async fn max_tokens_continuation_rechecks_hard_fit_and_compacts_first() {
    let history = Message::user_text("history ".repeat(180));
    let current = Message::user_text("current");
    let partial = "p".repeat(320);
    let instruction =
        Message::user_text("Continue exactly where you stopped. Do not repeat completed content.");
    let first_messages = vec![history.clone(), current.clone()];
    let uncompact_second = vec![
        history.clone(),
        current.clone(),
        Message::assistant(vec![Block::Text {
            text: partial.clone(),
        }]),
        instruction.clone(),
    ];
    let compact_second = vec![
        Message::user_text("s"),
        current.clone(),
        Message::assistant(vec![Block::Text {
            text: partial.clone(),
        }]),
        instruction,
    ];
    let mut bounded = config();
    bounded.reserved_output_tokens = 16;
    let input_budget = estimated_input_tokens(&bounded, &first_messages)
        .max(estimated_input_tokens(&bounded, &compact_second));
    assert!(estimated_input_tokens(&bounded, &uncompact_second) > input_budget);
    bounded.context_window = Some(input_budget + bounded.reserved_output_tokens);
    let compactor = Arc::new(ShrinkingContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: partial.clone(),
        },
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let handle = HarnessActor::spawn(bounded, provider.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("max-tokens-hard-fit"),
            messages: first_messages,
        })
        .await
        .expect("continuation turn accepted")
        .wait()
        .await
        .expect("continuation outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(provider.requests()[1].messages, compact_second);
}

struct CompletingDispatcher;

#[async_trait]
impl ToolDispatcher for CompletingDispatcher {
    async fn execute(
        &self,
        _run_id: &haider_protocol::ids::RunId,
        _item_id: &haider_protocol::ids::ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "done".into(),
            truncated: false,
            artifact: None,
            cursor: None,
        }))
    }
}

#[tokio::test]
async fn provider_opaque_state_is_journaled_and_replayed_on_tool_follow_up() {
    let opaque = serde_json::json!({
        "id": "rs_sanitized",
        "type": "reasoning",
        "encrypted_content": "encrypted-synthetic-continuation",
        "summary": []
    });
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitProviderOpaque {
            provider: "openai".into(),
            data: opaque.clone(),
        },
        FakeStep::EmitToolCall {
            call_id: "call-opaque".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitText {
            text: "continued".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let actor_task = tokio::spawn(actor.run());
    let turn = handle
        .submit_turn(SubmitTurn::new("continue safely"))
        .await
        .expect("turn accepted");
    let outcome = turn.wait().await.expect("turn outcome");
    assert_eq!(outcome.finish_reason, FinishReason::EndTurn);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    Block::ProviderOpaque { provider, data }
                        if provider == "openai" && data == &opaque
                )
            })
        }),
        "the exact opaque continuation must enter the next TurnRequest"
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    let opaque_events = events
        .iter()
        .filter(|event| {
            matches!(
                typed(event),
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::Extension { ref kind, .. },
                    ..
                } | ItemEvent::Completed {
                    item: TurnItem::Extension { ref kind, .. },
                    ..
                }) if kind == "provider_opaque"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(opaque_events.len(), 2);
    assert!(opaque_events.iter().all(|event| {
        !event.render.ui
            && event.render.durable
            && event.render.prompt == haider_protocol::envelope::PromptRender::Verbatim
    }));
    assert_items_closed_before_terminal(&events);

    drop(handle);
    actor_task.await.expect("actor exits");
}

#[tokio::test]
async fn split_utf8_reassembles_in_completed_agent_message() {
    let (handle, store, _) = runtime(vec![
        FakeStep::SplitUtf8 {
            text: "A🌍B".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("utf8"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);

    let events = store.events(&SessionId::new(SESSION)).await;
    let completed = events.iter().find_map(|event| match typed(event) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::AgentMessage { text },
            ..
        }) => Some(text),
        _ => None,
    });
    assert_eq!(completed.as_deref(), Some("A🌍B"));
}

#[tokio::test]
async fn malformed_frame_is_errored_with_typed_error() {
    let (handle, store, _) = runtime(vec![FakeStep::MalformedFrame]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("break safely"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert_eq!(
        error.details.expect("typed details")["provider_error_kind"],
        "MalformedFrame"
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

#[tokio::test]
async fn provider_retry_classification_and_retry_after_survive_actor_boundary() {
    let provider = Arc::new(ImmediateErrorProvider {
        error: ProviderError::new(ProviderErrorKind::RateLimited, "rate limited")
            .with_retry_after_ms(Some(3_000)),
    });
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider, store);

    let outcome = handle
        .submit_turn(SubmitTurn::new("classify"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    let error = outcome.error.expect("typed error");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.retryable);
    let details = error.details.expect("provider details");
    assert_eq!(details["provider_error_kind"], "RateLimited");
    assert_eq!(details["retry_after_ms"], 3_000);
}

/// MUTATION CHECK: restore the old `min(RETRY_CEILING_MS)` clamp for an
/// explicit Retry-After. Expected failure: the second request starts before
/// the provider's 3s instruction expires. Verified by revert in W3c1.1.
#[tokio::test(start_paused = true)]
async fn explicit_retry_after_is_respected_beyond_computed_jitter_ceiling() {
    let (handle, _store, provider) = runtime(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::RateLimited,
            message: "wait three seconds".into(),
            retry_after_ms: Some(3_000),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let turn = handle
        .submit_turn(SubmitTurn::new("respect retry-after"))
        .await
        .expect("turn accepted");
    tokio::task::yield_now().await;
    assert_eq!(provider.requests().len(), 1);

    advance(Duration::from_millis(2_999)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.requests().len(), 1);

    advance(Duration::from_millis(1)).await;
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn retry_after_above_respect_cap_terminalizes_retryable_exhaustion() {
    let (handle, _store, provider) = runtime(vec![FakeStep::Error {
        kind: ProviderErrorKind::RateLimited,
        message: "wait too long".into(),
        retry_after_ms: Some(60_001),
    }]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("cap retry-after"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("retryable exhaustion");
    assert!(error.retryable);
    assert!(error.message.contains("respect cap"));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn provider_error_after_first_event_is_never_retried() {
    let (handle, _store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "observable".into(),
        },
        FakeStep::Error {
            kind: ProviderErrorKind::Transport,
            message: "stream broke".into(),
            retry_after_ms: Some(1),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("do not duplicate output"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
}

/// MUTATION CHECK (W5c.1 safe-boundary rotation): clear the logical-turn
/// consumed flag after A→B, leave it clear after policy returns `Wait`,
/// initialize it false after an initial rotation, or allow resolver entry
/// after `provider_event_seen`. Expected failure: this test observes a second
/// resolver call/C request, a missing pre-B durable event, or replay after
/// text/reasoning/tool output.
/// Verified by revert (live second hop, repeated wait policy, initial second
/// hop, post-event rotation, and legacy single-account usage rejection) on
/// 2026-07-29.
#[tokio::test(start_paused = true)]
async fn rotation_is_once_pre_first_event_and_durable_before_the_alternate() {
    let rate_limit = || {
        ProviderError::new(ProviderErrorKind::RateLimited, "bounded rate limit")
            .with_retry_after_ms(Some(0))
    };

    let durable_store = Arc::new(MemoryStore::new());
    let primary = Arc::new(CountingOpeningErrorProvider::new(rate_limit()));
    let alternate = Arc::new(RotationAwareFinishProvider {
        store: Arc::clone(&durable_store),
        requests: AtomicUsize::new(0),
        saw_rotation_before_request: AtomicBool::new(false),
    });
    let forbidden_second = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let resolver = Arc::new(ScriptedRotationResolver {
        calls: AtomicUsize::new(0),
        first: alternate.clone(),
        first_alias: CredentialAlias::new("account-b"),
        second: forbidden_second.clone(),
        second_alias: CredentialAlias::new("account-c"),
    });
    let mut durable_config = config();
    durable_config.usage_account = Some(CredentialAlias::new("account-a"));
    durable_config.provider_attempt_resolver = Some(resolver.clone());
    let durable_handle =
        HarnessActor::spawn(durable_config, primary.clone(), durable_store.clone());
    let durable_outcome = durable_handle
        .submit_turn(SubmitTurn::new("rotate before output"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(durable_outcome.state, RunState::Done);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(primary.requests.load(Ordering::SeqCst), 1);
    assert_eq!(alternate.requests.load(Ordering::SeqCst), 1);
    assert!(alternate.saw_rotation_before_request.load(Ordering::SeqCst));
    assert!(forbidden_second.requests().is_empty());
    assert_eq!(
        durable_store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .filter(|event| matches!(typed(event), EventPayload::Rotation(_)))
            .count(),
        1
    );

    let once_store = Arc::new(MemoryStore::new());
    let once_primary = Arc::new(CountingOpeningErrorProvider::new(rate_limit()));
    let once_alternate = Arc::new(CountingOpeningErrorProvider::new(rate_limit()));
    let once_forbidden = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let once_resolver = Arc::new(ScriptedRotationResolver {
        calls: AtomicUsize::new(0),
        first: once_alternate.clone(),
        first_alias: CredentialAlias::new("once-b"),
        second: once_forbidden.clone(),
        second_alias: CredentialAlias::new("once-c"),
    });
    let mut once_config = config();
    once_config.usage_account = Some(CredentialAlias::new("once-a"));
    once_config.provider_attempt_resolver = Some(once_resolver.clone());
    let once_handle = HarnessActor::spawn(once_config, once_primary, once_store.clone());
    let once_outcome = once_handle
        .submit_turn(SubmitTurn::new("never rotate twice"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(once_outcome.state, RunState::Errored);
    assert_eq!(once_resolver.calls.load(Ordering::SeqCst), 1);
    assert!(once_forbidden.requests().is_empty());
    assert_eq!(
        once_store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .filter(|event| matches!(typed(event), EventPayload::Rotation(_)))
            .count(),
        1
    );

    let wait_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::RateLimited,
            message: "no usable alternate".into(),
            retry_after_ms: Some(0),
        },
        FakeStep::Error {
            kind: ProviderErrorKind::RateLimited,
            message: "still no usable alternate".into(),
            retry_after_ms: Some(0),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let wait_resolver = Arc::new(WaitingAttemptResolver {
        calls: AtomicUsize::new(0),
    });
    let mut wait_config = config();
    wait_config.usage_account = Some(CredentialAlias::new("wait-a"));
    wait_config.provider_attempt_resolver = Some(wait_resolver.clone());
    let wait_handle = HarnessActor::spawn(
        wait_config,
        wait_provider.clone(),
        Arc::new(MemoryStore::new()),
    );
    let wait_outcome = wait_handle
        .submit_turn(SubmitTurn::new("consult policy once without an alternate"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(wait_outcome.state, RunState::Done);
    assert_eq!(wait_resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(wait_provider.requests().len(), 3);

    let initial_store = Arc::new(MemoryStore::new());
    let initial_provider = Arc::new(CountingOpeningErrorProvider::inspecting(
        rate_limit(),
        Arc::clone(&initial_store),
    ));
    let initial_forbidden = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let initial_resolver = Arc::new(ScriptedRotationResolver {
        calls: AtomicUsize::new(0),
        first: initial_forbidden.clone(),
        first_alias: CredentialAlias::new("initial-c"),
        second: initial_forbidden.clone(),
        second_alias: CredentialAlias::new("initial-d"),
    });
    let mut initial_config = config();
    initial_config.usage_account = Some(CredentialAlias::new("initial-b"));
    initial_config.initial_rotation = Some(RotationEvent {
        provider: "fake".into(),
        from: CredentialAlias::new("initial-a"),
        to: CredentialAlias::new("initial-b"),
        cause: RotationCause::RateLimit,
    });
    initial_config.rotation_budget_consumed = true;
    initial_config.provider_attempt_resolver = Some(initial_resolver.clone());
    let initial_handle =
        HarnessActor::spawn(initial_config, initial_provider.clone(), initial_store);
    let initial_outcome = initial_handle
        .submit_turn(SubmitTurn::new("initial hop spends budget"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(initial_outcome.state, RunState::Errored);
    assert_eq!(initial_resolver.calls.load(Ordering::SeqCst), 0);
    assert!(initial_forbidden.requests().is_empty());
    assert!(
        initial_provider
            .saw_rotation_before_request
            .load(Ordering::SeqCst)
    );

    for script in [
        vec![
            FakeStep::EmitText {
                text: "visible".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::RateLimited,
                message: "after text".into(),
                retry_after_ms: Some(0),
            },
        ],
        vec![
            FakeStep::EmitReasoning {
                text: "visible reasoning".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::RateLimited,
                message: "after reasoning".into(),
                retry_after_ms: Some(0),
            },
        ],
        vec![
            FakeStep::EmitToolCallStart {
                call_id: "call-boundary".into(),
                name: "inspect".into(),
            },
            FakeStep::EmitToolArgsDelta {
                call_id: "call-boundary".into(),
                fragment: "{\"path\":\"src\"".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::RateLimited,
                message: "after tool delta".into(),
                retry_after_ms: Some(0),
            },
        ],
    ] {
        let partial = Arc::new(FakeProvider::new(script));
        let forbidden = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }]));
        let partial_resolver = Arc::new(ScriptedRotationResolver {
            calls: AtomicUsize::new(0),
            first: forbidden.clone(),
            first_alias: CredentialAlias::new("partial-b"),
            second: forbidden.clone(),
            second_alias: CredentialAlias::new("partial-c"),
        });
        let partial_store = Arc::new(MemoryStore::new());
        let mut partial_config = config();
        partial_config.usage_account = Some(CredentialAlias::new("partial-a"));
        partial_config.provider_attempt_resolver = Some(partial_resolver.clone());
        let partial_handle =
            HarnessActor::spawn(partial_config, partial.clone(), partial_store.clone());
        let partial_outcome = partial_handle
            .submit_turn(SubmitTurn::new("surface partial output honestly"))
            .await
            .expect("turn accepted")
            .wait()
            .await
            .expect("outcome");
        assert_eq!(partial_outcome.state, RunState::Errored);
        assert_eq!(partial.requests().len(), 1);
        assert_eq!(partial_resolver.calls.load(Ordering::SeqCst), 0);
        assert!(forbidden.requests().is_empty());
        assert!(
            !partial_store
                .events(&SessionId::new(SESSION))
                .await
                .iter()
                .any(|event| matches!(typed(event), EventPayload::Rotation(_)))
        );
    }

    let usage_store = Arc::new(MemoryStore::new());
    let usage_primary = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: Usage {
                input: 10,
                output: 4,
                reasoning: 2,
                cached: 1,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
            },
        },
        FakeStep::EmitToolCall {
            call_id: "usage-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Error {
            kind: ProviderErrorKind::RateLimited,
            message: "rotate on the second provider request".into(),
            retry_after_ms: Some(0),
        },
    ]));
    let usage_alternate = Arc::new(FakeProvider::new(vec![
        FakeStep::ExpectToolResult {
            call_id: "usage-tool".into(),
        },
        FakeStep::EmitUsage {
            usage: Usage {
                input: 6,
                output: 3,
                reasoning: 1,
                cached: 2,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
            },
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let usage_forbidden = Arc::new(FakeProvider::new(Vec::new()));
    let usage_resolver = Arc::new(ScriptedRotationResolver {
        calls: AtomicUsize::new(0),
        first: usage_alternate.clone(),
        first_alias: CredentialAlias::new("usage-b"),
        second: usage_forbidden.clone(),
        second_alias: CredentialAlias::new("usage-c"),
    });
    let mut usage_config = config();
    usage_config.usage_account = Some(CredentialAlias::new("usage-a"));
    usage_config.provider_attempt_resolver = Some(usage_resolver.clone());
    let (usage_actor, usage_handle) = HarnessActor::new_with_dispatcher(
        usage_config,
        usage_primary.clone(),
        usage_store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let _usage_actor_task = tokio::spawn(usage_actor.run());
    let usage_outcome = usage_handle
        .submit_turn(SubmitTurn::new("attribute both accounts"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(usage_outcome.state, RunState::Done);
    assert_eq!(usage_resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(usage_primary.requests().len(), 2);
    assert_eq!(usage_alternate.requests().len(), 1);
    assert!(usage_forbidden.requests().is_empty());
    let cumulative = usage_store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Usage(usage) => Some(usage),
            _ => None,
        })
        .next_back()
        .expect("cumulative usage");
    assert_eq!(cumulative.input, 16);
    assert_eq!(cumulative.output, 7);
    assert_eq!(cumulative.reasoning, 3);
    assert_eq!(cumulative.cached, 3);
    assert_eq!(cumulative.account, None);
    assert_eq!(
        cumulative
            .accounts
            .iter()
            .map(|usage| (usage.account.as_str(), usage.input, usage.output))
            .collect::<Vec<_>>(),
        vec![("usage-a", 10, 4), ("usage-b", 6, 3)]
    );
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_provider_retry_backoff_without_second_request() {
    let (handle, store, provider) = runtime(vec![
        FakeStep::Error {
            kind: ProviderErrorKind::Overloaded,
            message: "back off".into(),
            retry_after_ms: Some(60_000),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut events = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel backoff"))
        .await
        .expect("turn accepted");
    loop {
        let envelope = events.recv().await.expect("waiting event");
        if matches!(
            typed(&envelope),
            EventPayload::RunState(RunState::Waiting { .. })
        ) {
            break;
        }
    }
    turn.cancel();
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert_eq!(provider.requests().len(), 1);
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn provider_error_mid_tool_completes_failed_item_before_errored() {
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitToolCallStart {
            call_id: "call-open".into(),
            name: "inspect".into(),
        },
        FakeStep::MalformedFrame,
    ]);
    let outcome = handle
        .submit_turn(SubmitTurn::new("open then fail"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);

    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    assert!(events.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item:
                    TurnItem::ToolCall {
                        ref call_id,
                        status: ToolStatus::Failed,
                        ..
                    },
                ..
            }) if call_id == "call-open"
        )
    }));
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

/// MUTATION CHECK: split `commit_terminal_error` back into separate
/// `commit_payload(RunFailed)` and `commit_state(Errored)` calls. Expected
/// failure: no append batch contains the adjacent durable failure terminal.
/// Verified by revert on 2026-07-27.
#[tokio::test]
async fn provider_failure_commits_run_failed_and_errored_in_one_batch() {
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::MalformedFrame]));
    let store = Arc::new(BatchRecordingStore::new());
    let handle = HarnessActor::spawn(config(), provider, store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("fail atomically"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);

    let terminal_batches = store
        .batches()
        .into_iter()
        .filter(|batch| {
            batch
                .iter()
                .any(|payload| matches!(payload, EventPayload::RunState(RunState::Errored)))
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_batches.len(), 1);
    assert!(matches!(
        terminal_batches[0].as_slice(),
        [
            EventPayload::RunFailed { .. },
            EventPayload::RunState(RunState::Errored)
        ]
    ));
}

#[tokio::test]
async fn cancellation_mid_tool_completes_tool_as_cancelled_never_failed() {
    // Frozen law: cancellation is an outcome, never a failure — an open tool at
    // cancel time closes with ToolStatus::Cancelled (review r2 regression).
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitToolCallStart {
            call_id: "c-1".into(),
            name: "fs_read".into(),
        },
        FakeStep::EmitToolArgsDelta {
            call_id: "c-1".into(),
            fragment: "{\"pa".into(),
        },
        FakeStep::Delay { ms: 300 },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel mid tool"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(
                typed(&event),
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::ToolCall { .. },
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .expect("tool started");
    turn.cancel();
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    let tool_terminal = events.iter().find_map(|event| match typed(event) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall { status, .. },
            ..
        }) => Some(status),
        _ => None,
    });
    assert_eq!(
        tool_terminal,
        Some(haider_protocol::item::ToolStatus::Cancelled),
        "open tool must close as Cancelled, never Failed"
    );
}

#[tokio::test]
async fn cancellation_mid_stream_is_terminal_and_discards_later_events() {
    let (handle, store, _) = runtime(vec![
        FakeStep::EmitText {
            text: "first".into(),
        },
        FakeStep::Delay { ms: 200 },
        FakeStep::EmitText {
            text: "second".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel me"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(typed(&event), EventPayload::Item(ItemEvent::Delta { .. })) {
                break;
            }
        }
    })
    .await
    .expect("first delta arrives");
    turn.cancel();
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert_eq!(outcome.finish_reason, FinishReason::Cancelled);
    assert!(outcome.error.is_none());

    let before = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&before);
    assert!(before.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { ref text },
                ..
            }) if text == "first"
        )
    }));
    assert!(matches!(
        before.last().map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
    assert!(!before.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Delta {
                delta: haider_protocol::item::ItemDelta::Text { ref text },
                ..
            }) if text == "second"
        )
    }));
    sleep(Duration::from_millis(250)).await;
    let after = store.events(&SessionId::new(SESSION)).await;
    assert_eq!(after.len(), before.len(), "nothing follows Cancelled");
}

#[tokio::test]
async fn cancellation_during_finish_item_close_wins_before_done() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "selected finish".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(BlockingCompletedStore::new());
    let handle = HarnessActor::spawn(config(), provider, store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel finish"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), store.blocked.notified())
        .await
        .expect("item completion reached store");
    turn.cancel();
    store.release.notify_one();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("turn terminates")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);

    let events = store.inner.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn cancellation_interrupts_provider_stream_startup() {
    let provider = Arc::new(HangingStartProvider {
        entered: Notify::new(),
    });
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("cancel startup"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), provider.entered.notified())
        .await
        .expect("provider startup entered");
    turn.cancel();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("startup cancellation terminates")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[tokio::test]
async fn hanging_provider_can_always_be_cancelled() {
    let (handle, store, _) = runtime(vec![FakeStep::Hang]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("hang"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(typed(&event), EventPayload::RunState(RunState::Streaming)) {
                break;
            }
        }
    })
    .await
    .expect("streaming state arrives");

    turn.cancel();
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("cancel completes")
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

/// MUTATION CHECK: defer `Nudge` as a fresh logical turn or omit the
/// next-request-boundary drain. Expected runtime failure: the provider sees
/// one request only, or its second request lacks the daemon steer text.
#[tokio::test]
async fn daemon_nudge_reaches_the_next_safe_provider_boundary_in_the_same_turn() {
    let (handle, _store, provider) = runtime(vec![
        FakeStep::Delay { ms: 30 },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "nudge acknowledged".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("initial child work"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("event");
            if matches!(typed(&event), EventPayload::RunState(RunState::Streaming)) {
                break;
            }
        }
    })
    .await
    .expect("streaming state arrives");

    handle
        .nudge("report your status or conclude")
        .expect("nudge accepted without blocking the actor");
    let outcome = timeout(Duration::from_secs(1), turn.wait())
        .await
        .expect("nudged turn completes")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(block, Block::Text { text } if text == "report your status or conclude")
        })
    }));
}

#[tokio::test]
async fn submit_flood_is_bounded_and_cannot_starve_provider_progress() {
    let provider = Arc::new(FairnessProvider::new());
    let store = Arc::new(MemoryStore::new());
    let mut actor_config = config();
    actor_config.command_capacity = 64;
    actor_config.deferred_command_capacity = 4;
    let handle = HarnessActor::spawn(actor_config, provider.clone(), store);
    let first = handle
        .submit_turn(SubmitTurn::new("active stream"))
        .await
        .expect("first turn accepted");
    timeout(Duration::from_secs(1), provider.first_started.notified())
        .await
        .expect("provider stream starts");

    let stop = Arc::new(AtomicBool::new(false));
    let busy_count = Arc::new(AtomicUsize::new(0));
    let first_busy = Arc::new(Mutex::new(None));
    let mut flooders = Vec::new();
    for worker in 0..12 {
        let handle = handle.clone();
        let stop = Arc::clone(&stop);
        let busy_count = Arc::clone(&busy_count);
        let first_busy = Arc::clone(&first_busy);
        flooders.push(tokio::spawn(async move {
            let mut submission = 0usize;
            while !stop.load(Ordering::Acquire) {
                submission = submission.saturating_add(1);
                match handle
                    .submit_turn(SubmitTurn::new(format!("flood-{worker}-{submission}")))
                    .await
                {
                    Ok(turn) => {
                        let _ = turn.wait().await;
                    }
                    Err(error) if error.code == ErrorCode::Busy => {
                        busy_count.fetch_add(1, Ordering::Relaxed);
                        let mut observed = first_busy.lock().unwrap_or_else(|e| e.into_inner());
                        if observed.is_none() {
                            *observed = Some(error);
                        }
                    }
                    Err(_) => return,
                }
            }
        }));
    }

    timeout(Duration::from_secs(1), async {
        while busy_count.load(Ordering::Relaxed) < 100 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded queue surfaces busy rejections under flood");
    let busy = first_busy
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("typed busy error");
    assert_eq!(busy.code, ErrorCode::Busy);
    assert_eq!(
        busy.details.expect("busy details")["deferred_command_capacity"],
        4
    );

    provider.release.notify_one();
    let outcome = timeout(Duration::from_secs(1), first.wait())
        .await
        .expect("provider progress is not starved by command flood")
        .expect("first outcome");
    assert_eq!(outcome.state, RunState::Done);

    stop.store(true, Ordering::Release);
    for flooder in flooders {
        flooder.abort();
        let _ = flooder.await;
    }
}

#[tokio::test]
async fn actor_restarts_do_not_reuse_run_or_item_ids() {
    let store = Arc::new(MemoryStore::new());
    let script = || {
        vec![
            FakeStep::EmitText { text: "id".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]
    };
    let first = HarnessActor::spawn(
        config(),
        Arc::new(FakeProvider::new(script())),
        store.clone(),
    );
    let mut restarted_config = config();
    restarted_config.worker_generation += 1;
    let second = HarnessActor::spawn(
        restarted_config,
        Arc::new(FakeProvider::new(script())),
        store.clone(),
    );

    first
        .submit_turn(SubmitTurn::new("first actor"))
        .await
        .expect("first accepted")
        .wait()
        .await
        .expect("first outcome");
    second
        .submit_turn(SubmitTurn::new("second actor"))
        .await
        .expect("second accepted")
        .wait()
        .await
        .expect("second outcome");

    let events = store.events(&SessionId::new(SESSION)).await;
    let run_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| event.run_id.as_ref().map(ToString::to_string))
        .collect();
    let item_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Started { item_id, .. }) => Some(item_id.to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(run_ids.len(), 2);
    assert!(
        run_ids
            .iter()
            .any(|id| id.starts_with("run-session-test-23-"))
    );
    assert!(
        run_ids
            .iter()
            .any(|id| id.starts_with("run-session-test-24-"))
    );
    assert_eq!(item_ids.len(), 2);
    assert!(
        item_ids
            .iter()
            .any(|id| id.starts_with("item-session-test-23-"))
    );
    assert!(
        item_ids
            .iter()
            .any(|id| id.starts_with("item-session-test-24-"))
    );
}

#[tokio::test]
async fn memory_store_allocates_and_reads_committed_sequences() {
    let (handle, store, _) = runtime(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]);
    handle
        .submit_turn(SubmitTurn::new("store"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");

    assert_eq!(
        StoreHandle::latest_seq(store.as_ref(), &SessionId::new(SESSION))
            .await
            .expect("latest"),
        6
    );
    let tail = StoreHandle::read(store.as_ref(), &SessionId::new(SESSION), 3, 10)
        .await
        .expect("read");
    assert_eq!(
        tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![4, 5, 6]
    );
}

#[test]
fn completed_tool_call_is_pending_execution() {
    let item = TurnItem::ToolCall {
        call_id: "call".into(),
        name: "tool".into(),
        args: serde_json::json!({}),
        status: ToolStatus::Pending,
    };
    assert!(matches!(
        item,
        TurnItem::ToolCall {
            status: ToolStatus::Pending,
            ..
        }
    ));
}

struct BatchRecordingStore {
    inner: MemoryStore,
    batches: Mutex<Vec<Vec<EventPayload>>>,
}

impl BatchRecordingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            batches: Mutex::new(Vec::new()),
        }
    }

    fn batches(&self) -> Vec<Vec<EventPayload>> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl StoreHandle for BatchRecordingStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let batch = envelopes
            .iter()
            .map(|envelope| {
                serde_json::from_value(envelope.payload.clone()).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("recorded payload did not decode: {error}"),
                        false,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(batch);
        self.inner.append(envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }
}

struct BlockingCompletedStore {
    inner: MemoryStore,
    did_block: AtomicBool,
    blocked: Notify,
    release: Notify,
}

impl BlockingCompletedStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            did_block: AtomicBool::new(false),
            blocked: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl StoreHandle for BlockingCompletedStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let should_block = envelopes.first().is_some_and(|envelope| {
            envelope.payload["type"] == "item" && envelope.payload["event"] == "completed"
        });
        if should_block && !self.did_block.swap(true, Ordering::SeqCst) {
            self.blocked.notify_one();
            self.release.notified().await;
        }
        self.inner.append(envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }
}

struct HangingStartProvider {
    entered: Notify,
}

struct ImmediateErrorProvider {
    error: ProviderError,
}

struct CountingOpeningErrorProvider {
    error: ProviderError,
    requests: AtomicUsize,
    store: Option<Arc<MemoryStore>>,
    saw_rotation_before_request: AtomicBool,
}

impl CountingOpeningErrorProvider {
    fn new(error: ProviderError) -> Self {
        Self {
            error,
            requests: AtomicUsize::new(0),
            store: None,
            saw_rotation_before_request: AtomicBool::new(false),
        }
    }

    fn inspecting(error: ProviderError, store: Arc<MemoryStore>) -> Self {
        Self {
            error,
            requests: AtomicUsize::new(0),
            store: Some(store),
            saw_rotation_before_request: AtomicBool::new(false),
        }
    }
}

struct RotationAwareFinishProvider {
    store: Arc<MemoryStore>,
    requests: AtomicUsize,
    saw_rotation_before_request: AtomicBool,
}

struct ScriptedRotationResolver {
    calls: AtomicUsize,
    first: Arc<dyn Provider>,
    first_alias: CredentialAlias,
    second: Arc<dyn Provider>,
    second_alias: CredentialAlias,
}

#[derive(Debug)]
struct WaitingAttemptResolver {
    calls: AtomicUsize,
}

#[async_trait]
impl ProviderAttemptResolver for WaitingAttemptResolver {
    async fn resolve(
        &self,
        _current_account: &CredentialAlias,
        _error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderAttemptDecision::Wait)
    }
}

impl std::fmt::Debug for ScriptedRotationResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScriptedRotationResolver")
            .field("calls", &self.calls.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderAttemptResolver for ScriptedRotationResolver {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (provider, account) = if call == 0 {
            (Arc::clone(&self.first), self.first_alias.clone())
        } else {
            (Arc::clone(&self.second), self.second_alias.clone())
        };
        Ok(ProviderAttemptDecision::Rotate(ResolvedProviderAttempt {
            provider,
            account: account.clone(),
            rotation: RotationEvent {
                provider: "fake".into(),
                from: current_account.clone(),
                to: account,
                cause: if error.kind == ProviderErrorKind::RateLimited {
                    RotationCause::RateLimit
                } else {
                    RotationCause::Error
                },
            },
        }))
    }
}

#[async_trait]
impl Provider for ImmediateErrorProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        Err(self.error.clone())
    }
}

#[async_trait]
impl Provider for CountingOpeningErrorProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if let Some(store) = &self.store {
            let saw_rotation = store
                .events(&SessionId::new(SESSION))
                .await
                .iter()
                .any(|event| matches!(typed(event), EventPayload::Rotation(_)));
            self.saw_rotation_before_request
                .store(saw_rotation, Ordering::SeqCst);
        }
        Err(self.error.clone())
    }
}

#[async_trait]
impl Provider for RotationAwareFinishProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let saw_rotation = self
            .store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .any(|event| matches!(typed(event), EventPayload::Rotation(_)));
        self.saw_rotation_before_request
            .store(saw_rotation, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(haider_protocol::provider::StreamEvent::Finish {
                reason: FinishReason::EndTurn,
            }))
            .await
            .expect("finish receiver");
        Ok(receiver.into())
    }
}

struct FairnessProvider {
    first_started: Arc<Notify>,
    release: Arc<Notify>,
    requests: AtomicUsize,
}

impl FairnessProvider {
    fn new() -> Self {
        Self {
            first_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            requests: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for HangingStartProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.entered.notify_one();
        pending().await
    }
}

#[async_trait]
impl Provider for FairnessProvider {
    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        let request = self.requests.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(4);
        if request == 0 {
            let started = Arc::clone(&self.first_started);
            let release = Arc::clone(&self.release);
            tokio::spawn(async move {
                started.notify_one();
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::TextDelta {
                        text: "started".into(),
                    }))
                    .await;
                release.notified().await;
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::Finish {
                        reason: FinishReason::EndTurn,
                    }))
                    .await;
            });
        } else {
            tokio::spawn(async move {
                let _ = sender
                    .send(Ok(haider_protocol::provider::StreamEvent::Finish {
                        reason: FinishReason::EndTurn,
                    }))
                    .await;
            });
        }
        Ok(receiver.into())
    }
}
