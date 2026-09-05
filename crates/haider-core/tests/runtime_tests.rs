#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    ArtifactReader, CommittedRange, ContextCompactionError, ContextCompactionRequest,
    ContextCompactor, FinalizationGuard, FinalizationGuardDecision, HarnessActor, HarnessConfig,
    HarnessHandle, MemoryStore, PromptHistoryCompiler, ProviderAttemptDecision,
    ProviderAttemptResolver, ProviderBudgetGuard, ProviderBudgetGuardError, ProviderBudgetPermit,
    ProviderPairSwitch, ProviderPairSwitchCause, ProviderPairSwitchCommitter,
    ProviderPairSwitchTarget, ProviderRebindResolver, ProviderRebindTarget,
    ResolvedProviderAttempt, RouteWaitCheckpoint, RouteWaitTextCheckpoint, RouteWaitToolCheckpoint,
    StoreHandle, SubmitCommittedTurn, SubmitRouteWaitTurn, SubmitTurn, ToolDispatchResult,
    ToolDispatcher, VISION_IMAGE_ESTIMATE_TOKENS, estimate_provider_request_input_tokens,
};
use haider_platform::RouteStatus;
use haider_protocol::EventPayload;
use haider_protocol::branch::BranchDescriptor;
use haider_protocol::cache::CACHE_REQUEST_ATTEMPT_EXTENSION_KIND;
use haider_protocol::context::{
    CONTEXT_SAVINGS_EXTENSION_KIND, ContextCompactionTier, ContextFootprint, ContextFootprintTruth,
    ContextSavingsEvent,
};
use haider_protocol::credential::{RotationCause, RotationEvent};
use haider_protocol::envelope::{PromptRender, RawEnvelope};
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::history::{
    COMPACTION_INTENT_EXTENSION_KIND, CONTINUATION_CHECKPOINT_EXTENSION_KIND, CompactionIntent,
    CompactionResume, ContinuationCheckpoint,
};
use haider_protocol::ids::{
    ArtifactRef, BranchId, CredentialAlias, DeviceId, EventId, GraphId, ItemId, MenuId, NodeId,
    RunId, SessionId,
};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use haider_protocol::provider::{Block, CapabilityDoc, FinishReason, Usage, UsageSource};
use haider_protocol::state::{RunState, WaitReason};
use haider_protocol::tool::{
    AttachmentBlock, BoundedResult, ImageBlockRef, PdfDeliveryMode,
    TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN,
};
use haider_provider::{
    FakeProvider, FakeStep, Message, Provider, ProviderError, ProviderErrorKind, ProviderStream,
    ResolvedAttachment, TurnRequest,
};
use std::collections::{HashSet, VecDeque};
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
    serde_json::from_value(envelope.payload.clone().into()).expect("known payload")
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

fn completed_footprint(envelope: &RawEnvelope) -> Option<ContextFootprint> {
    match typed(envelope) {
        EventPayload::Item(ItemEvent::Completed { item, .. }) => {
            ContextFootprint::from_extension_item(&item)
        }
        _ => None,
    }
}

fn normalize(mut payload: serde_json::Value) -> serde_json::Value {
    fn mask_keyed_hashes(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) if text.starts_with("blake3-keyed:") => {
                *text = "<hash>".into();
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    mask_keyed_hashes(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    mask_keyed_hashes(value);
                }
            }
            _ => {}
        }
    }

    if payload["type"] == "item" {
        payload["item_id"] = serde_json::Value::String("<item>".into());
    }
    if payload["type"] == "node_committed" {
        payload["node"] = serde_json::Value::String("<node>".into());
        if payload.get("parent").is_some() {
            payload["parent"] = serde_json::Value::String("<node>".into());
        }
    }
    if payload["type"] == "usage" {
        // Cache-domain instrumentation includes run-scoped digests. This
        // sequence law compares the stable event contract; CM1h covers the
        // digest values themselves.
        if let Some(object) = payload.as_object_mut() {
            object.remove("scope");
            object.remove("cache_cost");
        }
    }
    mask_keyed_hashes(&mut payload);
    payload
}

fn assert_items_closed_before_terminal(events: &[RawEnvelope]) {
    let mut open = HashSet::<ItemId>::new();
    let mut finished_requests = HashSet::new();
    let mut saw_terminal = false;
    for event in events {
        assert!(!saw_terminal, "an envelope followed the terminal run state");
        match typed(event) {
            EventPayload::Item(ItemEvent::Started { item_id, .. }) => {
                assert!(open.insert(item_id), "an item started twice");
            }
            EventPayload::Item(ItemEvent::Completed {
                item_id,
                item: TurnItem::Extension { kind, data },
            }) if kind == "provider_round_terminal_v1" => {
                assert!(open.remove(&item_id), "completed item was not open");
                // Terminal metadata obeys the same paired item lifecycle and
                // additionally pins one exact Finish per physical request.
                let request: haider_protocol::cache::ProviderRequestAttemptV1 =
                    serde_json::from_value(event.payload["provider_request"].clone())
                        .expect("terminal fact request correlation");
                assert!(request.coordinates_valid());
                assert_eq!(request.session_id, event.session_id);
                assert_eq!(Some(&request.run_id), event.run_id.as_ref());
                assert!(finished_requests.insert((
                    request.run_id,
                    request.turn_ordinal,
                    request.request_ordinal,
                )));
                let reason: FinishReason =
                    serde_json::from_value(event.payload["provider_finish_reason"].clone())
                        .expect("terminal fact Finish");
                assert_eq!(data, serde_json::json!({"reason": reason}));
                assert_eq!(
                    serde_json::to_value(event.render).expect("render"),
                    serde_json::json!({"ui": false, "durable": true, "prompt": "omit"})
                );
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

#[derive(Debug)]
struct ScriptedFinalizationGuard {
    decisions: Mutex<VecDeque<FinalizationGuardDecision>>,
    calls: AtomicUsize,
}

#[async_trait]
impl FinalizationGuard for ScriptedFinalizationGuard {
    async fn before_done(&self, _run_id: &RunId) -> Result<FinalizationGuardDecision, HaiderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.decisions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| {
                HaiderError::new(
                    ErrorCode::Internal,
                    "scripted finalization guard exhausted",
                    false,
                )
            })
    }
}

#[derive(Debug)]
struct FailingFinalizationGuard {
    error: HaiderError,
}

#[derive(Debug)]
struct CancellingBeforeRequestBudgetGuard;

#[async_trait]
impl ProviderBudgetGuard for CancellingBeforeRequestBudgetGuard {
    async fn before_request(
        &self,
        _run_id: &RunId,
        _provider: &str,
        _request: &TurnRequest,
        _projected_input_tokens: u64,
    ) -> Result<ProviderBudgetPermit, ProviderBudgetGuardError> {
        Err(ProviderBudgetGuardError::Cancelled)
    }

    async fn after_usage(&self, _run_id: &RunId) -> Result<(), ProviderBudgetGuardError> {
        Ok(())
    }

    async fn after_route_interruption(
        &self,
        _run_id: &RunId,
    ) -> Result<(), ProviderBudgetGuardError> {
        Ok(())
    }

    async fn after_request(
        &self,
        _run_id: &RunId,
        _provider: &str,
        _model: &str,
        _usage_reported: bool,
    ) -> Result<(), ProviderBudgetGuardError> {
        Ok(())
    }
}

#[tokio::test]
async fn budget_guard_pre_request_cancellation_never_opens_provider_or_errors() {
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let mut runtime_config = config();
    runtime_config.provider_budget_guard = Some(Arc::new(CancellingBeforeRequestBudgetGuard));
    let handle = HarnessActor::spawn(runtime_config, provider.clone(), store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("cancel before provider admission"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");

    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(outcome.error.is_none());
    assert!(provider.requests().is_empty());
    assert!(matches!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .last()
            .map(typed),
        Some(EventPayload::RunState(RunState::Cancelled))
    ));
}

#[async_trait]
impl FinalizationGuard for FailingFinalizationGuard {
    async fn before_done(&self, _run_id: &RunId) -> Result<FinalizationGuardDecision, HaiderError> {
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn finalization_failure_journals_top_level_workflow_unfinished_without_parking() {
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let mut runtime_config = config();
    runtime_config.finalization_guard = Some(Arc::new(FailingFinalizationGuard {
        error: HaiderError::new(
            ErrorCode::WorkflowUnfinished,
            "workflow remains unfinished",
            false,
        ),
    }));
    let handle = HarnessActor::spawn(runtime_config, provider, store.clone());

    let outcome = timeout(
        Duration::from_secs(2),
        handle
            .submit_turn(SubmitTurn::new("finish"))
            .await
            .expect("turn accepted")
            .wait(),
    )
    .await
    .expect("finalization failure is bounded")
    .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("typed failure").code,
        ErrorCode::WorkflowUnfinished
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(events.iter().any(|event| matches!(
        typed(event),
        EventPayload::RunFailed {
            code: ErrorCode::WorkflowUnfinished,
            ..
        }
    )));
    assert!(!events.iter().any(|event| matches!(
        typed(event),
        EventPayload::RunState(RunState::InputRequired { .. })
            | EventPayload::RunState(RunState::PermissionRequired { .. })
    )));
    assert!(matches!(
        events.last().map(typed),
        Some(EventPayload::RunState(RunState::Errored))
    ));
}

/// Expected failure under mutation: moving the guard after `Done`, dropping
/// the reminder continuation, or treating the blocking card as advisory lets
/// this turn terminalize before durable abandonment authority is observed.
#[tokio::test]
async fn m2c_end_turn_defers_once_then_waits_for_committed_abandonment() {
    let reminder = "continue unfinished graph obligations".to_string();
    let menu = Menu {
        id: MenuId::new("m2c-finalization-menu"),
        kind: MenuKind::GraphAbandonConfirm {
            graph_id: GraphId::new("m2c-finalization-graph"),
            run_id: RunId::new("placeholder-run"),
            state_digest: "state-digest".into(),
        },
        title: "Unfinished workflow".into(),
        body: Vec::new(),
        options: vec![
            MenuOption {
                key: "continue-work".into(),
                label: "Continue work".into(),
                detail: None,
                decision: None,
            },
            MenuOption {
                key: "abandon-and-finish".into(),
                label: "Abandon and finish".into(),
                detail: None,
                decision: None,
            },
        ],
        blocking: true,
        scope: MenuScope::Session,
        origin: "convergence-graph".into(),
        ttl_ms: None,
        timeout_option: None,
    };
    let guard = Arc::new(ScriptedFinalizationGuard {
        decisions: Mutex::new(VecDeque::from([
            FinalizationGuardDecision::Continue {
                reminder: Some(reminder.clone()),
            },
            FinalizationGuardDecision::ConfirmRequired(menu.clone()),
            FinalizationGuardDecision::AllowDone,
        ])),
        calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let mut runtime_config = config();
    runtime_config.finalization_guard = Some(guard.clone());
    let handle = HarnessActor::spawn(runtime_config, provider.clone(), store.clone());
    let turn = handle
        .submit_turn(SubmitTurn::new("finish only when graph authority permits"))
        .await
        .expect("turn accepted");
    let mut states = handle.state_receiver();
    let parked = states
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("guard parks on durable confirmation")
        .clone();
    assert_eq!(
        parked,
        Some(RunState::InputRequired {
            menu: menu.id.clone()
        })
    );
    let history = store.events(&SessionId::new(SESSION)).await;
    assert!(
        !history
            .iter()
            .any(|event| { matches!(typed(event), EventPayload::RunState(RunState::Done)) })
    );
    assert_eq!(provider.requests().len(), 2);
    assert!(
        provider.requests()[1]
            .messages
            .contains(&Message::user_text(reminder))
    );

    let mut answer = [RawEnvelope {
        event_id: EventId::new("m2c-committed-abandon-answer"),
        seq: 0,
        committed_at_ms: 0,
        causation_id: None,
        payload: serde_json::to_value(EventPayload::MenuAnswered(MenuAnswer {
            menu: menu.id,
            option_key: Some("abandon-and-finish".into()),
            option_index: 1,
            value: None,
            via: AnswerVia::Rpc,
        }))
        .expect("answer serializes")
        .into(),
        ..history.last().expect("run history").clone()
    }];
    store
        .append(&mut answer)
        .await
        .expect("answer commits first");
    handle
        .apply_committed_menu_event(answer[0].clone())
        .expect("committed answer wakes guard");
    assert_eq!(
        turn.wait().await.expect("turn completes").state,
        RunState::Done
    );
    assert_eq!(guard.calls.load(Ordering::Relaxed), 3);
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
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
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
    assert_eq!(outcome.state, RunState::Done, "outcome: {outcome:?}");
    assert_eq!(outcome.finish_reason, FinishReason::ToolUse);

    let events = store.events(&SessionId::new(SESSION)).await;
    let actual: Vec<_> = events
        .iter()
        .map(|event| normalize(event.payload.clone().into()))
        .collect();
    let correlation_run_id = events
        .iter()
        .find_map(|event| event.run_id.as_ref())
        .expect("turn event run id");
    let mut expected = vec![
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
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"cache_request_attempt_v1",
                "data":{
                    "ordinal":1,
                    "correlation":{
                        "request_kind":"primary",
                        "request_ordinal":1,
                        "run_id":correlation_run_id.as_str(),
                        "session_id":SESSION,
                        "turn_ordinal":1
                    },
                    "diagnostic":{
                        "stable_prefix_tokens":3,
                        "history_message_count":0,
                        "breakpoint_hashes":{
                            "system":"<hash>",
                            "tools":"<hash>",
                            "history":"<hash>"
                        },
                        "cache_domain_hash":"<hash>",
                        "prefix_match":{"state":"unavailable"},
                        "control":{"state":"unavailable"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"cache_request_attempt_v1",
                "data":{
                    "ordinal":1,
                    "correlation":{
                        "request_kind":"primary",
                        "request_ordinal":1,
                        "run_id":correlation_run_id.as_str(),
                        "session_id":SESSION,
                        "turn_ordinal":1
                    },
                    "diagnostic":{
                        "stable_prefix_tokens":3,
                        "history_message_count":0,
                        "breakpoint_hashes":{
                            "system":"<hash>",
                            "tools":"<hash>",
                            "history":"<hash>"
                        },
                        "cache_domain_hash":"<hash>",
                        "prefix_match":{"state":"unavailable"},
                        "control":{"state":"unavailable"}
                    }
                }
            }
        }),
        // One visible budget item shares the request-attempt append. Keep
        // both lifecycle halves in the exact projection rather than filtering
        // the new telemetry out of this journal contract pin.
        serde_json::json!({
            "type":"item", "event":"started", "item_id":"<item>",
            "item":{
                "item":"extension", "kind":"provider_request_budget_v1",
                "data":{
                    "used":1, "budget":{"tranche":32,"hard_cap":64},
                    "phase":"progress",
                    "continuation":{"session_id":SESSION,"run_id":correlation_run_id.as_str()}
                }
            }
        }),
        serde_json::json!({
            "type":"item", "event":"completed", "item_id":"<item>",
            "item":{
                "item":"extension", "kind":"provider_request_budget_v1",
                "data":{
                    "used":1, "budget":{"tranche":32,"hard_cap":64},
                    "phase":"progress",
                    "continuation":{"session_id":SESSION,"run_id":correlation_run_id.as_str()}
                }
            }
        }),
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
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_start",
                        "call_id":"call-1",
                        "name":"inspect"
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_start",
                        "call_id":"call-1",
                        "name":"inspect"
                    }
                }
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
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_args_delta",
                        "call_id":"call-1",
                        "args_fragment":"{\"path\":\"src/lib.rs\"}"
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_args_delta",
                        "call_id":"call-1",
                        "args_fragment":"{\"path\":\"src/lib.rs\"}"
                    }
                }
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
        serde_json::json!({
            "type":"item",
            "event":"started",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_end",
                        "call_id":"call-1"
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"item",
            "event":"completed",
            "item_id":"<item>",
            "item":{
                "item":"extension",
                "kind":"haider.route_replay_event.v1",
                "data":{
                    "response_epoch":0,
                    "stream_event":{
                        "event":"tool_call_end",
                        "call_id":"call-1"
                    }
                }
            }
        }),
        serde_json::json!({
            "type":"usage",
            "input":11,
            "output":7,
            "reasoning":2,
            "cached":3,
            "source":"provider_reported",
            "request":{
                "ordinal":1,
                "input":11,
                "output":7,
                "reasoning":2,
                "cached":3,
                "source":"provider_reported",
                "cache":{
                    "stable_prefix_tokens":3,
                    "history_message_count":0,
                    "breakpoint_hashes":{
                        "system":"<hash>",
                        "tools":"<hash>",
                        "history":"<hash>"
                    },
                    "cache_domain_hash":"<hash>",
                    "prefix_match":{"state":"unavailable"},
                    "control":{"state":"unavailable"},
                    "classification":{"class":"unavailable"}
                }
            }
        }),
        serde_json::json!({
            "type":"item", "event":"started", "item_id":"<item>",
            "item":{"item":"extension", "kind":"provider_round_terminal_v1",
                "data":{"reason":"tool_use"}}
        }),
        serde_json::json!({
            "type":"item", "event":"completed", "item_id":"<item>",
            "item":{"item":"extension", "kind":"provider_round_terminal_v1",
                "data":{"reason":"tool_use"}},
            "provider_finish_reason":"tool_use"
        }),
        serde_json::json!({"type":"run_state","state":"done", "provider_finish_reason":"tool_use"}),
    ];
    let correlation = serde_json::json!({
        "session_id": SESSION,
        "run_id": correlation_run_id,
        "turn_ordinal": 1,
        "request_ordinal": 1,
        "request_kind": "primary"
    });
    // This one-request fixture activates correlation at Thinking, after its
    // three acceptance events. Compare every additive field without stripping
    // metadata from the actual stream.
    for payload in &mut expected[3..] {
        payload["provider_request"] = correlation.clone();
    }
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
            Block::Text { text } => Some(text.to_owned_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(second_text.iter().any(|text| text == "partial answer"));
    assert!(
        second_text
            .iter()
            .any(|text| text
                == "Continue exactly where you stopped. Do not repeat completed content.")
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
    expected_covered: Option<Vec<Message>>,
}

#[async_trait]
impl ContextCompactor for FakeContextCompactor {
    async fn plan(
        &self,
        _run_id: &RunId,
        resume_cause: CompactionResume,
        _messages: &[Message],
        current_turn_start: usize,
    ) -> Result<haider_core::PlannedContextCompaction, HaiderError> {
        Ok(haider_core::PlannedContextCompaction {
            intent: CompactionIntent {
                operation_id: "forced-test-compaction".into(),
                covers_from: NodeId::new("old-root"),
                covers_to: NodeId::new("old-head"),
                resume_cause,
            },
            covered_message_count: current_turn_start,
        })
    }

    async fn compact(
        &self,
        request: ContextCompactionRequest<'_>,
    ) -> Result<haider_core::ContextCompactionOutcome, ContextCompactionError> {
        let ContextCompactionRequest {
            covered_messages,
            economy_before,
            ..
        } = request;
        self.calls.fetch_add(1, Ordering::Relaxed);
        let default_expected = [Message::user_text("old history")];
        assert_eq!(
            covered_messages.as_slice(),
            self.expected_covered
                .as_deref()
                .unwrap_or(&default_expected)
        );
        let (economy, _) = economy_before.record(
            haider_protocol::context::ContextCompactionTier::Summarize,
            100,
            10,
        );
        Ok(haider_core::ContextCompactionOutcome {
            summary: Message::user_text("compacted history"),
            economy,
        })
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
        _messages: &[Message],
        current_turn_start: usize,
    ) -> Result<haider_core::PlannedContextCompaction, HaiderError> {
        Ok(haider_core::PlannedContextCompaction {
            intent: CompactionIntent {
                operation_id: "hard-fit-test-compaction".into(),
                covers_from: NodeId::new("old-root"),
                covers_to: NodeId::new("old-head"),
                resume_cause,
            },
            covered_message_count: current_turn_start,
        })
    }

    async fn compact(
        &self,
        request: ContextCompactionRequest<'_>,
    ) -> Result<haider_core::ContextCompactionOutcome, ContextCompactionError> {
        let ContextCompactionRequest {
            covered_messages,
            economy_before,
            ..
        } = request;
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(covered_messages.len(), 1);
        let (economy, _) = economy_before.record(
            haider_protocol::context::ContextCompactionTier::Summarize,
            100,
            1,
        );
        Ok(haider_core::ContextCompactionOutcome {
            summary: Message::user_text("s"),
            economy,
        })
    }
}

#[derive(Debug, Default)]
struct IneffectiveContextCompactor {
    calls: AtomicUsize,
}

#[derive(Debug, Default)]
struct RecordingPairSwitchCommitter {
    switches: Mutex<Vec<ProviderPairSwitch>>,
}

#[async_trait]
impl ProviderPairSwitchCommitter for RecordingPairSwitchCommitter {
    async fn commit(&self, switch: &ProviderPairSwitch) -> Result<(), HaiderError> {
        self.switches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(switch.clone());
        Ok(())
    }
}

#[async_trait]
impl ContextCompactor for IneffectiveContextCompactor {
    async fn plan(
        &self,
        _run_id: &RunId,
        resume_cause: CompactionResume,
        _messages: &[Message],
        current_turn_start: usize,
    ) -> Result<haider_core::PlannedContextCompaction, HaiderError> {
        Ok(haider_core::PlannedContextCompaction {
            intent: CompactionIntent {
                operation_id: "ineffective-test-compaction".into(),
                covers_from: NodeId::new("old-root"),
                covers_to: NodeId::new("old-head"),
                resume_cause,
            },
            covered_message_count: current_turn_start,
        })
    }

    async fn compact(
        &self,
        request: ContextCompactionRequest<'_>,
    ) -> Result<haider_core::ContextCompactionOutcome, ContextCompactionError> {
        let ContextCompactionRequest {
            covered_messages,
            economy_before,
            ..
        } = request;
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(covered_messages.len(), 1);
        let (economy, _) = economy_before.record(
            haider_protocol::context::ContextCompactionTier::Summarize,
            100,
            100,
        );
        Ok(haider_core::ContextCompactionOutcome {
            summary: covered_messages.into_iter().next().expect("covered prefix"),
            economy,
        })
    }
}

fn estimated_input_tokens(config: &HarnessConfig, messages: &[Message]) -> u64 {
    estimate_provider_request_input_tokens(
        messages,
        &config.system_prompt,
        &config.tools,
        &config.attachments,
    )
}

/// MUTATION CHECK: serialize resolved image bytes into request-token
/// accounting or remove the fixed vision charge. Expected RUNTIME failure:
/// the tiny and 5 MiB payload estimates diverge, or the image delta is not
/// the provider-neutral 1,600-token approximation.
#[test]
fn image_footprint_uses_fixed_vision_estimate_not_base64_length() {
    let artifact = ArtifactRef::new("blake3:image");
    let messages = vec![Message {
        role: haider_provider::MessageRole::User,
        blocks: vec![Block::Attachment(AttachmentBlock::Image {
            artifact: artifact.clone(),
            mime: "image/png".into(),
            width: None,
            height: None,
        })],
    }];
    let without_image = vec![Message {
        role: haider_provider::MessageRole::User,
        blocks: Vec::new(),
    }];
    let tiny = vec![ResolvedAttachment {
        artifact: artifact.clone(),
        data_base64: "AA==".into(),
    }];
    let five_mib = vec![ResolvedAttachment {
        artifact,
        data_base64: "A".repeat((5_usize * 1024 * 1024).div_ceil(3) * 4),
    }];

    let tiny_estimate = estimate_provider_request_input_tokens(&messages, &None, &[], &tiny);
    let large_estimate = estimate_provider_request_input_tokens(&messages, &None, &[], &five_mib);
    let baseline = estimate_provider_request_input_tokens(&without_image, &None, &[], &[]);

    assert_eq!(tiny_estimate, large_estimate);
    // LITERAL charge, not the constant: a self-referential assertion would
    // follow a mutated constant to zero and blind the footprint to images.
    assert_eq!(VISION_IMAGE_ESTIMATE_TOKENS, 1_600);
    assert!(tiny_estimate >= baseline + 1_600);
    assert!(tiny_estimate < baseline + 1_600 + 128);
}

/// MUTATION CHECK: ignore resolved attachments in request accounting and the
/// native-document delta falls from 2,048 projected tokens to zero.
#[test]
fn native_pdf_footprint_counts_resolved_document_request_bytes() {
    let artifact = ArtifactRef::new("blake3:native-pdf");
    let message = |delivery| Message {
        role: haider_provider::MessageRole::User,
        blocks: vec![Block::Attachment(AttachmentBlock::Pdf {
            artifact: artifact.clone(),
            name: "report.pdf".into(),
            pages: 1,
            delivery,
        })],
    };
    let resolved = vec![ResolvedAttachment {
        artifact: artifact.clone(),
        data_base64: "A".repeat(8_192),
    }];
    let native = vec![message(PdfDeliveryMode::NativeDocument)];
    let native_without_bytes = estimate_provider_request_input_tokens(&native, &None, &[], &[]);
    let native_with_bytes = estimate_provider_request_input_tokens(&native, &None, &[], &resolved);
    assert_eq!(native_with_bytes - native_without_bytes, 2_048);

    let duplicate_native = vec![
        message(PdfDeliveryMode::NativeDocument),
        message(PdfDeliveryMode::NativeDocument),
    ];
    assert_eq!(
        estimate_provider_request_input_tokens(&duplicate_native, &None, &[], &resolved)
            - estimate_provider_request_input_tokens(&duplicate_native, &None, &[], &[]),
        4_096,
        "providers inline the resolved bytes for every PDF block occurrence"
    );

    let extracted = vec![message(PdfDeliveryMode::ExtractedText)];
    assert_eq!(
        estimate_provider_request_input_tokens(&extracted, &None, &[], &resolved),
        estimate_provider_request_input_tokens(&extracted, &None, &[], &[]),
        "extracted text is already present in the message projection"
    );
}

#[test]
fn tool_image_footprint_uses_the_same_oldest_first_budget_as_the_request() {
    let messages = (0..=TOOL_RESULT_IMAGE_MAX_COUNT_PER_TURN)
        .map(|index| {
            Message::tool_result_with_images(
                format!("call-{index}"),
                "capture",
                false,
                vec![ImageBlockRef {
                    artifact: ArtifactRef::new(format!("blake3:image-{index}")),
                    media_type: "image/png".into(),
                    width: 1,
                    height: 1,
                    byte_len: 1,
                }],
            )
        })
        .collect::<Vec<_>>();
    let newest_suffix = messages[1..].to_vec();

    let over_budget = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    let retained = estimate_provider_request_input_tokens(&newest_suffix, &None, &[], &[]);

    assert!(
        over_budget < retained + 256,
        "only the bounded omission note differs"
    );
}

/// MUTATION CHECK: ignore the reserved output budget when deriving the soft
/// threshold. Expected runtime failure: the 40k reserve case stays at 170k
/// instead of moving down to 160k; the 30k meeting point is also pinned.
#[test]
fn soft_threshold_honors_eighty_five_percent_and_output_reserve() {
    assert_eq!(
        haider_core::context_soft_threshold_tokens(200_000, 30_000),
        Some(170_000)
    );
    assert_eq!(
        haider_core::context_soft_threshold_tokens(200_000, 40_000),
        Some(160_000)
    );
    assert_eq!(
        haider_core::context_soft_threshold_tokens(200_000, 200_000),
        Some(0)
    );
}

/// MUTATION CHECK: change `<` to `<=`, round the percentage before comparing,
/// or drop the independent hard-fit arm. Expected runtime failure: one of the
/// exact-boundary or over-budget cases changes its decision.
#[test]
fn compaction_guard_uses_exact_fifteen_percent_and_hard_fit_laws() {
    assert_eq!(haider_core::COMPACTION_MIN_FREED_PERCENT, 15);
    assert!(haider_core::compaction_guard_tripped(100, 86, 1_000));
    assert!(!haider_core::compaction_guard_tripped(100, 85, 1_000));
    assert!(!haider_core::compaction_guard_tripped(101, 85, 1_000));
    assert!(haider_core::compaction_guard_tripped(101, 86, 1_000));
    assert!(haider_core::compaction_guard_tripped(100, 50, 49));
}

/// Regression law: rolling out `compaction_guard_v1` must not weaken the v0
/// hard-fit check. With the feature off, an oversized post-compaction request
/// still fails before provider work.
#[tokio::test]
async fn feature_off_retains_post_compaction_hard_fit_error() {
    let messages = vec![
        Message::user_text("oversized history ".repeat(500)),
        Message::user_text("current"),
    ];
    let mut bounded = config();
    bounded.reserved_output_tokens = 1;
    let used = estimated_input_tokens(&bounded, &messages);
    bounded.context_window = Some(used / 2);
    assert!(!bounded.compaction_guard_v1);
    let compactor = Arc::new(IneffectiveContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let handle = HarnessActor::spawn(bounded, provider.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("feature-off-hard-fit"),
            messages,
        })
        .await
        .expect("hard-fit turn accepted")
        .wait()
        .await
        .expect("hard-fit turn outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert!(provider.requests().is_empty());
    assert!(
        outcome
            .error
            .expect("hard-fit error")
            .message
            .contains("compacted provider input estimate")
    );
}

/// MUTATION CHECK: omit the effectiveness arm or fall through to another
/// request/compaction. Expected runtime failure: the provider receives a
/// request, the turn succeeds, or the compactor is called more than once.
#[tokio::test]
async fn ineffective_compaction_without_promotion_errors_without_a_second_compaction() {
    let history = Message::user_text("ineffective history ".repeat(500));
    let messages = vec![history, Message::user_text("current")];
    let mut bounded = config();
    bounded.reserved_output_tokens = 1;
    let used = estimated_input_tokens(&bounded, &messages);
    let window = used.saturating_mul(100).saturating_add(84) / 85;
    bounded.context_window = Some(window);
    bounded.context_compaction_v1 = true;
    bounded.compaction_guard_v1 = true;
    assert!(used >= haider_core::context_soft_threshold_tokens(window, 1).expect("soft threshold"));
    assert!(used <= window.saturating_sub(1));

    let compactor = Arc::new(IneffectiveContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let handle = HarnessActor::spawn(bounded, provider.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("ineffective-compaction-guard"),
            messages,
        })
        .await
        .expect("guarded turn accepted")
        .wait()
        .await
        .expect("guarded turn outcome");

    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("honest context error");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.message.contains("ContextExceeded"));
    assert!(error.message.contains("context compaction guard"));
    assert!(error.message.contains("minimum reduction is 15%"));
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert!(provider.requests().is_empty());
}

/// MUTATION CHECK: skip the durable switch, accept a non-larger window, or
/// keep sending on the original provider. Expected runtime failure: the
/// receipt coordinates disappear or the request lands on the wrong lane.
#[tokio::test]
async fn ineffective_compaction_promotes_to_a_larger_same_provider_model() {
    let partial = "p".repeat(5_000);
    let messages = vec![
        Message::user_text("promotion history ".repeat(500)),
        Message::user_text("current"),
    ];
    let mut bounded = config();
    bounded.model = "model-small".into();
    bounded.usage_scope.provider = "fake-a".into();
    bounded.usage_scope.model = bounded.model.clone();
    bounded.usage_scope.cache_epoch = "epoch-small".into();
    bounded.usage_account = Some(CredentialAlias::new("fake-a-primary"));
    bounded.reserved_output_tokens = 1;
    let used = estimated_input_tokens(&bounded, &messages);
    let window = used.saturating_mul(100).saturating_add(84) / 85;
    let mut continued_messages = messages.clone();
    continued_messages.push(Message::assistant(vec![Block::Text {
        text: partial.clone().into(),
    }]));
    continued_messages.push(Message::user_text(
        "Continue exactly where you stopped. Do not repeat completed content.",
    ));
    let promoted_window = estimated_input_tokens(&bounded, &continued_messages).saturating_add(1);
    assert!(promoted_window > window);
    bounded.context_window = Some(window);
    bounded.context_compaction_v1 = true;
    bounded.compaction_guard_v1 = true;
    let compactor = Arc::new(IneffectiveContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());

    let original = Arc::new(FakeProvider::new(Vec::new()));
    let promoted = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText { text: partial },
        FakeStep::Finish {
            reason: FinishReason::MaxTokens,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    bounded.compaction_promotion = Some(ProviderPairSwitchTarget {
        provider: promoted.clone(),
        account: CredentialAlias::new("fake-a-primary"),
        provider_name: "fake-a".into(),
        model: "model-large".into(),
        context_window: Some(promoted_window),
        cached_input_is_subset: true,
        provider_request_state: Default::default(),
        auth_scope: "api_key".into(),
        attempt_resolver: None,
        cause: ProviderPairSwitchCause::CompactionGuard,
    });
    let committer = Arc::new(RecordingPairSwitchCommitter::default());
    bounded.provider_pair_switch_committer = Some(committer.clone());

    let handle = HarnessActor::spawn(bounded, original.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("ineffective-compaction-promotes"),
            messages,
        })
        .await
        .expect("promotion turn accepted")
        .wait()
        .await
        .expect("promotion turn outcome");

    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert!(original.requests().is_empty());
    let requests = promoted.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "model-large");
    assert_eq!(requests[1].model, "model-large");
    let switches = committer
        .switches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].from_provider, "fake-a");
    assert_eq!(switches[0].from_model, "model-small");
    assert_eq!(switches[0].to_provider, "fake-a");
    assert_eq!(switches[0].to_model, "model-large");
    assert_eq!(switches[0].cause, ProviderPairSwitchCause::CompactionGuard);
}

/// MUTATION CHECK: change the strict window ordering to `>=` or trust only
/// the model name. Expected runtime failure: the equal-window target commits
/// and receives provider work instead of ending with ContextExceeded.
#[tokio::test]
async fn compaction_promotion_refuses_a_model_without_a_larger_known_window() {
    let messages = vec![
        Message::user_text("non-larger history ".repeat(500)),
        Message::user_text("current"),
    ];
    let mut bounded = config();
    bounded.model = "model-small".into();
    bounded.usage_scope.provider = "fake-a".into();
    bounded.usage_scope.model = bounded.model.clone();
    bounded.reserved_output_tokens = 1;
    let used = estimated_input_tokens(&bounded, &messages);
    let window = used.saturating_mul(100).saturating_add(84) / 85;
    bounded.context_window = Some(window);
    bounded.context_compaction_v1 = true;
    bounded.compaction_guard_v1 = true;
    let compactor = Arc::new(IneffectiveContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());

    let promoted = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    bounded.compaction_promotion = Some(ProviderPairSwitchTarget {
        provider: promoted.clone(),
        account: CredentialAlias::new("fake-a-primary"),
        provider_name: "fake-a".into(),
        model: "model-not-larger".into(),
        context_window: Some(window),
        cached_input_is_subset: true,
        provider_request_state: Default::default(),
        auth_scope: "api_key".into(),
        attempt_resolver: None,
        cause: ProviderPairSwitchCause::CompactionGuard,
    });
    let committer = Arc::new(RecordingPairSwitchCommitter::default());
    bounded.provider_pair_switch_committer = Some(committer.clone());

    let original = Arc::new(FakeProvider::new(Vec::new()));
    let handle = HarnessActor::spawn(bounded, original.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("non-larger-promotion-refused"),
            messages,
        })
        .await
        .expect("refusal turn accepted")
        .wait()
        .await
        .expect("refusal turn outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert!(original.requests().is_empty());
    assert!(promoted.requests().is_empty());
    assert!(
        committer
            .switches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

/// MUTATION CHECK: enable W7b policy without the advertised feature. Expected
/// runtime failure: a request between the soft and hard lines compacts or
/// emits a footprint even though `context_compaction_v1` is disabled.
#[tokio::test]
async fn soft_threshold_and_footprints_are_feature_gated() {
    let history = Message::user_text("feature-gated ".repeat(200));
    let current = Message::user_text("current");
    let messages = vec![history.clone(), current];
    let mut gated = config();
    let used = estimated_input_tokens(&gated, &messages);
    gated.context_window = Some(used.saturating_add(10));
    gated.reserved_output_tokens = 1;
    assert!(
        used >= haider_core::context_soft_threshold_tokens(
            gated.context_window.expect("known window"),
            gated.reserved_output_tokens,
        )
        .expect("soft threshold")
    );
    let compactor = Arc::new(ShrinkingContextCompactor::default());
    gated.context_compactor = Some(compactor.clone());
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(gated, provider.clone(), store.clone());
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("feature-gated-soft-threshold"),
            messages,
        })
        .await
        .expect("feature-gated turn accepted")
        .wait()
        .await
        .expect("feature-gated turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 0);
    assert_eq!(provider.requests().len(), 1);
    assert!(provider.requests()[0].messages.contains(&history));
    assert!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .all(|event| completed_footprint(event).is_none())
    );
}

/// MUTATION CHECK: drop the W7b threshold check, hide the W7a intent, move
/// it after Compacting, or retain the stale pre-compaction footprint.
/// Expected runtime failure: compaction disappears, event ordering/render
/// changes, the provider sees the old prefix, or the reset does not shrink.
#[tokio::test]
async fn soft_threshold_preannounces_compacts_and_publishes_the_reset_before_provider() {
    let history = "history ".repeat(100_000);
    let compactor = Arc::new(ShrinkingContextCompactor::default());
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let mut bounded = config();
    bounded.context_window = Some(200_000);
    bounded.reserved_output_tokens = 30_000;
    bounded.context_compaction_v1 = true;
    bounded.context_compactor = Some(compactor.clone());
    let handle = HarnessActor::spawn(bounded, provider.clone(), store.clone());

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("soft-threshold-order"),
            messages: vec![
                Message::user_text(history.clone()),
                Message::user_text("current"),
            ],
        })
        .await
        .expect("threshold turn accepted")
        .wait()
        .await
        .expect("threshold turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert_eq!(provider.requests().len(), 1);
    assert!(
        !provider.requests()[0]
            .messages
            .contains(&Message::user_text(history))
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    let footprints = events
        .iter()
        .filter_map(|event| completed_footprint(event).map(|footprint| (event.seq, footprint)))
        .collect::<Vec<_>>();
    assert_eq!(footprints.len(), 2);
    let (before_seq, before) = &footprints[0];
    let (after_seq, after) = &footprints[1];
    assert_eq!(before.soft_threshold_tokens, Some(170_000));
    assert_eq!(before.truth, ContextFootprintTruth::Estimated);
    assert!(before.used_tokens >= 170_000);
    assert!(after.used_tokens < before.used_tokens);
    assert_eq!(after.truth, ContextFootprintTruth::Estimated);

    let intent = events
        .iter()
        .find(|event| completed_extension(event, COMPACTION_INTENT_EXTENSION_KIND))
        .expect("typed pre-announcement intent");
    assert!(intent.render.ui);
    assert_eq!(intent.render.prompt, PromptRender::Omit);
    let compacting_seq = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::RunState(RunState::Compacting) => Some(event.seq),
            _ => None,
        })
        .expect("compacting state");
    let streaming_seq = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::RunState(RunState::Streaming) => Some(event.seq),
            _ => None,
        })
        .expect("provider stream opened");
    assert!(*before_seq < intent.seq);
    assert!(intent.seq < compacting_seq);
    assert!(compacting_seq < *after_seq);
    assert!(*after_seq < streaming_seq);
}

#[tokio::test]
async fn fast_mode_structurally_trims_whole_stale_pairs_while_default_mode_does_not() {
    let mut messages = Vec::new();
    for ordinal in 0..30 {
        let call_id = format!("structural-runtime-{ordinal:02}");
        messages.push(Message::assistant(vec![Block::ToolCall {
            call_id: call_id.clone(),
            name: "read_file".into(),
            args: serde_json::json!({"path": format!("/{ordinal}")}),
        }]));
        messages.push(Message::tool_result(
            &call_id,
            format!("runtime-output-{ordinal:02}-{}", "r".repeat(8_192)),
            false,
        ));
    }
    messages.push(Message::user_text("current structural runtime turn"));
    let estimated_before = estimate_provider_request_input_tokens(&messages, &None, &[], &[]);
    assert!((60_000..75_000).contains(&estimated_before));

    let fast_provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let fast_store = Arc::new(MemoryStore::new());
    let mut fast = config();
    fast.context_window = Some(100_000);
    fast.reserved_output_tokens = 1_000;
    fast.context_compaction_v1 = true;
    fast.structural_context_trimming = true;
    let fast_handle = HarnessActor::spawn(fast, fast_provider.clone(), fast_store.clone());
    fast_handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("structural-runtime-fast"),
            messages: messages.clone(),
        })
        .await
        .expect("fast structural turn accepted")
        .wait()
        .await
        .expect("fast structural turn completes");
    let fast_requests = fast_provider.requests();
    let fast_request = &fast_requests[0];
    assert_eq!(
        fast_request
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter(|block| matches!(block, Block::ToolCall { .. }))
            .count(),
        24
    );
    assert_eq!(
        fast_request
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter(|block| matches!(block, Block::ToolResult { .. }))
            .count(),
        24
    );
    let savings = fast_store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind, data },
                ..
            }) if kind == CONTEXT_SAVINGS_EXTENSION_KIND => {
                serde_json::from_value::<ContextSavingsEvent>(data).ok()
            }
            _ => None,
        })
        .expect("durable structural savings event");
    let (tier, removed_tool_call_ids) = savings
        .conversation()
        .expect("structural saving is conversation-level");
    assert_eq!(tier, ContextCompactionTier::StructuralTrim24);
    assert_eq!(removed_tool_call_ids.len(), 6);
    assert!(savings.estimated_tokens_saved > 0);

    let default_provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let mut default = config();
    default.context_window = Some(100_000);
    default.reserved_output_tokens = 1_000;
    default.context_compaction_v1 = true;
    let default_handle = HarnessActor::spawn(
        default,
        default_provider.clone(),
        Arc::new(MemoryStore::new()),
    );
    default_handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("structural-runtime-default"),
            messages,
        })
        .await
        .expect("default structural turn accepted")
        .wait()
        .await
        .expect("default structural turn completes");
    assert_eq!(
        default_provider.requests()[0]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .filter(|block| matches!(block, Block::ToolCall { .. }))
            .count(),
        30
    );
}

/// MUTATION CHECK: substitute any default model window for `None`. Expected
/// runtime failure: the oversized unknown-window request auto-compacts or its
/// footprint fabricates a threshold/window.
#[tokio::test]
async fn unknown_window_publishes_an_estimate_but_never_auto_compacts() {
    let history = "unknown ".repeat(100_000);
    let compactor = Arc::new(ShrinkingContextCompactor::default());
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(MemoryStore::new());
    let mut unknown = config();
    unknown.context_window = None;
    unknown.reserved_output_tokens = u64::MAX;
    unknown.context_compaction_v1 = true;
    unknown.context_compactor = Some(compactor.clone());
    let handle = HarnessActor::spawn(unknown, provider.clone(), store.clone());

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("unknown-soft-threshold"),
            messages: vec![
                Message::user_text(history.clone()),
                Message::user_text("current"),
            ],
        })
        .await
        .expect("unknown-window turn accepted")
        .wait()
        .await
        .expect("unknown-window outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 0);
    assert!(
        provider.requests()[0]
            .messages
            .contains(&Message::user_text(history))
    );
    let footprint = store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .find_map(completed_footprint)
        .expect("estimated unknown-window footprint");
    assert_eq!(footprint.context_window, None);
    assert_eq!(footprint.soft_threshold_tokens, None);
    assert_eq!(footprint.truth, ContextFootprintTruth::Estimated);
}

/// MUTATION CHECK: mark every footprint exact, derive occupancy from the
/// cumulative Usage event, or add OpenAI cached input twice. Expected runtime
/// failure: the reported/no-usage truth markers or normalized splits differ.
#[tokio::test]
async fn footprint_is_exact_only_for_request_local_provider_usage() {
    let reported_store = Arc::new(MemoryStore::new());
    let reported_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: Usage {
                input: 100,
                output: 20,
                reasoning: 5,
                cached: 30,
                source: UsageSource::ProviderReported,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
            },
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let mut reported_config = config();
    reported_config.context_window = Some(1_000);
    reported_config.reserved_output_tokens = 100;
    reported_config.context_compaction_v1 = true;
    let reported = HarnessActor::spawn(reported_config, reported_provider, reported_store.clone());
    reported
        .submit_turn(SubmitTurn::new("measure"))
        .await
        .expect("reported turn")
        .wait()
        .await
        .expect("reported outcome");
    let reported_footprints = reported_store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter_map(completed_footprint)
        .collect::<Vec<_>>();
    assert_eq!(
        reported_footprints[0].truth,
        ContextFootprintTruth::Estimated
    );
    let exact = reported_footprints.last().expect("exact request footprint");
    assert_eq!(exact.truth, ContextFootprintTruth::Exact);
    assert_eq!(exact.input_tokens, 70);
    assert_eq!(exact.cached_input_tokens, 30);
    assert_eq!(exact.output_tokens, 20);
    assert_eq!(exact.used_tokens, 120);
    assert_eq!(exact.estimated_turns_to_threshold, Some(37));

    let estimated_store = Arc::new(MemoryStore::new());
    let estimated_provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage {
            usage: Usage {
                input: 900,
                output: 80,
                reasoning: 0,
                cached: 40,
                source: UsageSource::Estimated,
                account: None,
                accounts: Vec::new(),
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
            },
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let mut estimated_config = config();
    estimated_config.context_window = None;
    estimated_config.context_compaction_v1 = true;
    let estimated = HarnessActor::spawn(
        estimated_config,
        estimated_provider,
        estimated_store.clone(),
    );
    estimated
        .submit_turn(SubmitTurn::new("estimate"))
        .await
        .expect("estimated turn")
        .wait()
        .await
        .expect("estimated outcome");
    let last = estimated_store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter_map(completed_footprint)
        .next_back()
        .expect("estimated request footprint");
    assert_eq!(last.truth, ContextFootprintTruth::Estimated);
    assert_ne!(last.used_tokens, 1_020);
}

/// MUTATION CHECK: route ContextExceeded through generic retry or omit the
/// one-shot guard. Expected runtime failure: no CompactionIntent is durable,
/// the retry lacks the summary, or the double-overflow case makes >2 calls.
#[tokio::test]
async fn context_overflow_forces_one_compaction_and_only_one_retry() {
    let compactor = Arc::new(FakeContextCompactor {
        expected_covered: Some(vec![
            Message::user_text("old history one"),
            Message::user_text("old history two"),
        ]),
        ..FakeContextCompactor::default()
    });
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
    success_config.volatile_user_tail = Some("volatile snapshot".into());
    let success = HarnessActor::spawn(
        success_config,
        success_provider.clone(),
        success_store.clone(),
    );
    let outcome = success
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("forced-success"),
            messages: vec![
                Message::user_text("old history one"),
                Message::user_text("old history two"),
                Message::user_text("current"),
            ],
        })
        .await
        .expect("accepted forced-compaction run")
        .wait()
        .await
        .expect("forced-compaction outcome");
    assert_eq!(outcome.state, RunState::Done);
    let success_requests = success_provider.requests();
    assert_eq!(success_requests.len(), 2);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        success_requests[1].messages,
        vec![
            Message::user_text("compacted history"),
            Message::user_text("volatile snapshot"),
            Message::user_text("current"),
        ],
        "reactive compaction rebases the frozen snapshot before the current user"
    );
    let compacted_metadata = success_requests[1]
        .cache_metadata
        .as_ref()
        .expect("compacted request metadata");
    assert_eq!(
        (
            compacted_metadata.stable_history_end,
            compacted_metadata.current_user_start,
            compacted_metadata.latest_compaction_summary_end,
        ),
        (2, 2, Some(1)),
        "reactive compaction rebases every request-local cache boundary"
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
            text: partial.clone().into(),
        }]),
        instruction.clone(),
    ];
    let compact_second = vec![
        Message::user_text("s"),
        current.clone(),
        Message::assistant(vec![Block::Text {
            text: partial.clone().into(),
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

/// MUTATION CHECK: let the reactive overflow one-shot guard suppress later
/// proactive checks in the same logical turn. Expected runtime failure: the
/// MaxTokens continuation crosses the soft line but invokes the compactor
/// only once instead of immediately before both provider rounds.
#[tokio::test]
async fn proactive_compaction_can_repeat_after_continuation_growth() {
    let history = Message::user_text("history ".repeat(1_000));
    let current = Message::user_text("current");
    let partial = "p".repeat(5_000);
    let instruction =
        Message::user_text("Continue exactly where you stopped. Do not repeat completed content.");
    let initial_messages = vec![history, current.clone()];
    let after_first_compaction = vec![Message::user_text("s"), current.clone()];
    let continuation_messages = vec![
        Message::user_text("s"),
        current,
        Message::assistant(vec![Block::Text {
            text: partial.clone().into(),
        }]),
        instruction,
    ];

    let mut bounded = config();
    bounded.reserved_output_tokens = 1;
    let continuation_used = estimated_input_tokens(&bounded, &continuation_messages);
    bounded.context_window = Some(continuation_used.saturating_add(100));
    let soft_threshold = haider_core::context_soft_threshold_tokens(
        bounded.context_window.expect("known window"),
        bounded.reserved_output_tokens,
    )
    .expect("soft threshold");
    assert!(estimated_input_tokens(&bounded, &initial_messages) >= soft_threshold);
    assert!(estimated_input_tokens(&bounded, &after_first_compaction) < soft_threshold);
    assert!(continuation_used >= soft_threshold);
    assert!(
        continuation_used
            <= bounded
                .context_window
                .expect("known window")
                .saturating_sub(bounded.reserved_output_tokens)
    );

    let compactor = Arc::new(ShrinkingContextCompactor::default());
    bounded.context_compaction_v1 = true;
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
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(bounded, provider.clone(), store.clone());
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("repeat-proactive-compaction"),
            messages: initial_messages,
        })
        .await
        .expect("repeated proactive turn accepted")
        .wait()
        .await
        .expect("repeated proactive outcome");

    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 2);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(provider.requests()[0].messages, after_first_compaction);
    assert_eq!(provider.requests()[1].messages, continuation_messages);
    assert_eq!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .filter_map(completed_footprint)
            .count(),
        4
    );
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
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

struct CountingCompletingDispatcher {
    calls: AtomicUsize,
}

#[async_trait]
impl ToolDispatcher for CountingCompletingDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "done once".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

struct DelayedCompletingDispatcher {
    delay: Duration,
}

#[async_trait]
impl ToolDispatcher for DelayedCompletingDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        sleep(self.delay).await;
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "done after a long tool wait".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

/// HAIDER949(d). The fake stream queues usage behind a tool call, while the
/// actor spends six virtual minutes executing that tool. MUTATION CHECK:
/// restore the old `Instant::now()` usage-processing timestamp; request two
/// then reports a near-zero gap instead of the send-to-send interval.
#[tokio::test(start_paused = true)]
async fn reuse_gap_tracks_request_send_interval_across_long_tool_loop() {
    const TOOL_WAIT: Duration = Duration::from_secs(6 * 60);
    let usage = Usage {
        input: 1_024,
        output: 32,
        reasoning: 0,
        cached: 0,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "slow-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        },
        // This is queued immediately, but the actor cannot process it until
        // the blocking tool dispatch above has completed.
        FakeStep::EmitUsage { usage },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "slow-tool".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store,
        Some(Arc::new(DelayedCompletingDispatcher { delay: TOOL_WAIT })),
    );
    let actor_task = tokio::spawn(actor.run());

    let outcome = handle
        .submit_turn(SubmitTurn::new("run the slow tool"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let measured_gap = requests[1]
        .cache_metadata
        .as_ref()
        .expect("second request cache metadata")
        .reuse_gap_ms
        .expect("measured reuse gap");
    assert_eq!(
        measured_gap,
        u64::try_from(TOOL_WAIT.as_millis()).expect("fixture duration fits u64"),
        "reuse gap must span request send to request send, including tool execution"
    );

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}

struct LargeResultDispatcher {
    preview: String,
}

#[async_trait]
impl ToolDispatcher for LargeResultDispatcher {
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
            preview: self.preview.clone(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

struct ForgedImageDispatcher {
    image: ImageBlockRef,
}

#[async_trait]
impl ToolDispatcher for ForgedImageDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "forged image".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: vec![self.image.clone()],
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

struct GenericArtifactReader {
    artifact: ArtifactRef,
    bytes: Vec<u8>,
}

#[async_trait]
impl ArtifactReader for GenericArtifactReader {
    async fn read_artifact(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, HaiderError> {
        if artifact == &self.artifact {
            Ok(self.bytes.clone())
        } else {
            Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                "missing artifact fixture",
                false,
            ))
        }
    }
}

#[tokio::test]
async fn forged_generic_image_ref_fails_before_any_tool_result_is_journaled() {
    let bytes = b"generic CAS bytes, not an image".to_vec();
    let artifact = ArtifactRef::new(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let image = ImageBlockRef {
        artifact: artifact.clone(),
        media_type: "image/png".into(),
        width: 1,
        height: 1,
        byte_len: bytes.len() as u64,
    };
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "forged-image-call".into(),
            name: "forged_image".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let mut harness_config = config();
    harness_config.tool_result_images_supported = true;
    let (actor, handle) = HarnessActor::new_with_dispatcher_and_artifacts(
        harness_config,
        provider,
        store.clone(),
        Some(Arc::new(ForgedImageDispatcher { image })),
        Some(Arc::new(GenericArtifactReader { artifact, bytes })),
    );
    let actor_task = tokio::spawn(actor.run());

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("forged-image-run"),
            messages: vec![Message::user_text("capture")],
        })
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn terminates");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(ErrorCode::StoreCorrupt)
    );
    assert!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .all(|event| {
                !matches!(
                    serde_json::from_value::<EventPayload>(event.payload.clone().into()),
                    Ok(EventPayload::ToolResult { .. })
                )
            })
    );
    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}

/// MUTATION CHECK: run the soft check only at logical-turn start. Expected
/// runtime failure: the tool-expanded second request retains the old prefix,
/// no pre-request compaction occurs, or no reset footprint is published.
#[tokio::test]
async fn tool_round_crossing_soft_threshold_compacts_before_the_next_request() {
    let history = Message::user_text("history ".repeat(800));
    let current = Message::user_text("current");
    let preview = "tool-result".repeat(160);
    let first_messages = vec![history.clone(), current.clone()];
    let second_messages = vec![
        history.clone(),
        current.clone(),
        Message::assistant(vec![Block::ToolCall {
            call_id: "threshold-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        }]),
        Message::tool_result("threshold-tool", preview.clone(), false),
    ];
    let mut bounded = config();
    bounded.reserved_output_tokens = 1;
    let first_used = estimated_input_tokens(&bounded, &first_messages);
    let second_used = estimated_input_tokens(&bounded, &second_messages);
    let desired_threshold = first_used.saturating_add(1);
    let window = desired_threshold.saturating_mul(100).saturating_add(84) / 85;
    bounded.context_window = Some(window);
    bounded.context_compaction_v1 = true;
    let threshold =
        haider_core::context_soft_threshold_tokens(window, 1).expect("known-window threshold");
    assert!(first_used < threshold);
    assert!(second_used >= threshold);

    let compactor = Arc::new(ShrinkingContextCompactor::default());
    bounded.context_compactor = Some(compactor.clone());
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "threshold-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        bounded,
        provider.clone(),
        store.clone(),
        Some(Arc::new(LargeResultDispatcher { preview })),
    );
    let actor_task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: RunId::new("tool-soft-threshold"),
            messages: first_messages,
        })
        .await
        .expect("tool threshold turn accepted")
        .wait()
        .await
        .expect("tool threshold outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(compactor.calls.load(Ordering::Relaxed), 1);
    assert_eq!(provider.requests().len(), 2);
    assert!(!provider.requests()[1].messages.contains(&history));

    let footprints = store
        .events(&SessionId::new(SESSION))
        .await
        .iter()
        .filter_map(completed_footprint)
        .collect::<Vec<_>>();
    assert_eq!(footprints.len(), 3);
    assert!(footprints[0].used_tokens < threshold);
    assert!(footprints[1].used_tokens >= threshold);
    assert!(footprints[2].used_tokens < footprints[1].used_tokens);

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
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
    assert_eq!(
        completed.map(|text| text.to_owned_string()).as_deref(),
        Some("A🌍B")
    );
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

// v0.0.970 intentionally replaces the old terminal-on-first-malformed pin:
// malformed calls still never dispatch, and their failure remains durable;
// only the first consecutive malformed call may request a correction.
fn malformed_tool_steps(call_id: &str, arguments: &str) -> Vec<FakeStep> {
    vec![
        FakeStep::EmitToolCallStart {
            call_id: call_id.into(),
            name: "inspect".into(),
        },
        FakeStep::EmitToolArgsDelta {
            call_id: call_id.into(),
            fragment: arguments.into(),
        },
        FakeStep::EmitToolCallEnd {
            call_id: call_id.into(),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]
}

async fn toolrepair_run(
    cfg: HarnessConfig,
    script: Vec<FakeStep>,
) -> (
    haider_core::TurnOutcome,
    Vec<RawEnvelope>,
    Vec<TurnRequest>,
    usize,
) {
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        cfg,
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("repair the tool call"))
        .await
        .expect("submit")
        .wait()
        .await
        .expect("outcome");
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    handle.stop().await.expect("stop");
    task.await.expect("actor joined");
    (
        outcome,
        events,
        provider.requests(),
        dispatcher.calls.load(Ordering::SeqCst),
    )
}

#[tokio::test]
async fn malformed_tool_json_is_durable_invalid_result_with_one_repair_continuation() {
    let mut script = malformed_tool_steps("bad-1", "{broken");
    script.extend([
        FakeStep::ExpectToolResult {
            call_id: "bad-1".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let (outcome, events, requests, calls) = toolrepair_run(config(), script).await;
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(requests.len(), 2);
    assert_eq!(calls, 0, "malformed arguments never dispatch");
    let (result_index, result) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match typed(event) {
            EventPayload::ToolResult { call_id, result } if call_id == "bad-1" => {
                Some((index, result))
            }
            _ => None,
        })
        .expect("durable invalid result");
    assert_eq!(
        result.status,
        haider_protocol::tool::ToolResultStatus::Failed
    );
    assert_eq!(
        serde_json::to_value(result.data.as_ref().expect("typed data")).expect("serialize")["kind"],
        "invalid_tool_call"
    );
    assert_eq!(
        result
            .presentation
            .as_ref()
            .expect("presentation")
            .subcode
            .as_str(),
        "invalid-tool-call"
    );
    assert!(result.preview.contains("key must be a string"));
    assert!(result.preview.contains("inspect"));
    assert!(events[result_index + 1..].iter().any(|event| matches!(typed(event),
        EventPayload::Item(ItemEvent::Completed { item: TurnItem::ToolCall { args, status: ToolStatus::Failed, .. }, .. })
            if args == serde_json::json!("{broken")
    )));
    assert!(requests[1].messages.iter().flat_map(|message| &message.blocks).any(|block| matches!(block,
        Block::ToolCall { call_id, args, .. } if call_id == "bad-1" && args == &serde_json::json!({})
    )));
    assert!(requests[1].messages.iter().flat_map(|message| &message.blocks).any(|block| matches!(block,
        Block::ToolResult { call_id, preview, .. } if call_id == "bad-1" && preview == &result.preview
    )));
    let next_request_index = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            completed_extension(event, CACHE_REQUEST_ATTEMPT_EXTENSION_KIND).then_some(index)
        })
        .next_back()
        .expect("durable repair attempt");
    assert!(
        result_index < next_request_index,
        "result is durable before repair attempt"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(typed(event), EventPayload::RunFailed { .. }))
    );
}

#[tokio::test]
async fn second_consecutive_malformed_tool_json_terminates_after_one_repair() {
    let mut script = malformed_tool_steps("bad-1", "{broken");
    script.extend(malformed_tool_steps("bad-2", "["));
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let (outcome, events, requests, calls) = toolrepair_run(config(), script).await;
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(requests.len(), 2, "no second repair continuation");
    assert_eq!(calls, 0);
    assert_eq!(events.iter().filter(|event| matches!(typed(event), EventPayload::ToolResult { ref result, .. }
        if result.presentation.as_ref().is_some_and(|value| value.subcode.as_str() == "invalid-tool-call")
    )).count(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(typed(event), EventPayload::RunFailed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcome
            .error
            .expect("typed failure")
            .details
            .expect("provider details")["provider_error_kind"],
        "MalformedFrame"
    );
}

#[tokio::test]
async fn two_malformed_calls_in_one_response_terminate_without_a_repair_send() {
    let mut script = malformed_tool_steps("bad-1", "{");
    script.pop();
    script.extend(malformed_tool_steps("bad-2", "{"));
    let (outcome, _, requests, calls) = toolrepair_run(config(), script).await;
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(requests.len(), 1);
    assert_eq!(calls, 0);
}

#[tokio::test]
async fn valid_tool_call_resets_malformed_repair_allowance() {
    let mut script = malformed_tool_steps("bad-1", "{broken");
    script.extend([
        FakeStep::EmitToolCall {
            call_id: "good".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    script.extend(malformed_tool_steps("bad-2", "{broken"));
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let (outcome, _, requests, calls) = toolrepair_run(config(), script).await;
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(requests.len(), 4);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn non_object_tool_arguments_are_repairable_invalid_calls() {
    for arguments in ["null", "[]", "42", r#""text""#] {
        let mut script = malformed_tool_steps("bad", arguments);
        script.push(FakeStep::Finish {
            reason: FinishReason::EndTurn,
        });
        let (outcome, events, requests, calls) = toolrepair_run(config(), script).await;
        assert_eq!(outcome.state, RunState::Done);
        assert_eq!(requests.len(), 2);
        assert_eq!(calls, 0);
        assert!(events.iter().any(
            |event| matches!(typed(event), EventPayload::ToolResult { result, .. }
                if result.preview.contains("expected a JSON object")
            )
        ));
    }
}

fn toolrepair_config(names: &[&str]) -> HarnessConfig {
    let mut cfg = config();
    cfg.enforce_advertised_tool_ceiling = true;
    cfg.tools = names
        .iter()
        .map(|name| haider_provider::ToolDefinition {
            name: (*name).into(),
            description: "test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect();
    cfg
}

#[tokio::test]
async fn tool_name_case_and_underscore_repair_is_reported_in_durable_and_live_result() {
    for requested in ["FS_READ", "FsRead", "fs__read"] {
        let (outcome, events, requests, calls) = toolrepair_run(
            toolrepair_config(&["fs_read"]),
            vec![
                FakeStep::EmitToolCall {
                    call_id: "repair-name".into(),
                    name: requested.into(),
                    args: serde_json::json!({}),
                },
                FakeStep::Finish {
                    reason: FinishReason::ToolUse,
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        )
        .await;
        assert_eq!(outcome.state, RunState::Done);
        assert_eq!(calls, 1);
        let preview = events
            .iter()
            .find_map(|event| match typed(event) {
                EventPayload::ToolResult { result, .. } => Some(result.preview),
                _ => None,
            })
            .expect("durable result");
        let value: serde_json::Value = serde_json::from_str(&preview).expect("correction JSON");
        assert_eq!(value["tool_name_correction"]["requested"], requested);
        assert_eq!(value["tool_name_correction"]["resolved"], "fs_read");
        assert!(
            requests[1]
                .messages
                .iter()
                .flat_map(|message| &message.blocks)
                .any(|block| matches!(block,
                    Block::ToolResult { preview: text, .. } if text == &preview
                ))
        );
        assert!(events.iter().any(
            |event| matches!(typed(event), EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::ToolCall { name, status: ToolStatus::Completed, .. }, ..
        }) if name == "fs_read")
        ));
    }
}

#[tokio::test]
async fn tool_name_repair_does_not_resolve_ambiguous_or_unadvertised_names() {
    for requested in ["F_SREAD", "FS_WRITE"] {
        let (_, events, _, calls) = toolrepair_run(
            toolrepair_config(&["fs_read", "fsread"]),
            vec![
                FakeStep::EmitToolCall {
                    call_id: "denied".into(),
                    name: requested.into(),
                    args: serde_json::json!({}),
                },
                FakeStep::Finish {
                    reason: FinishReason::ToolUse,
                },
                FakeStep::Finish {
                    reason: FinishReason::EndTurn,
                },
            ],
        )
        .await;
        assert_eq!(calls, 0);
        assert!(events.iter().any(|event| matches!(typed(event), EventPayload::ToolResult { result, .. }
            if result.status == haider_protocol::tool::ToolResultStatus::Rejected && result.preview.contains("grant_ceiling_violation")
        )));
    }
    let (_, events, _, calls) = toolrepair_run(
        toolrepair_config(&["fs_read", "fsread"]),
        vec![
            FakeStep::EmitToolCall {
                call_id: "exact".into(),
                name: "fs_read".into(),
                args: serde_json::json!({}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    assert_eq!(
        calls, 1,
        "exact spelling wins even with normalized collision"
    );
    assert!(!events.iter().any(
        |event| matches!(typed(event), EventPayload::ToolResult { result, .. }
            if result.preview.contains("tool_name_correction")
        )
    ));
}

#[tokio::test]
async fn malformed_tool_result_replays_with_the_same_provider_safe_arguments() {
    let mut script = malformed_tool_steps("bad-replay", "{broken");
    script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let (_, mut events, requests, _) = toolrepair_run(config(), script).await;
    let store = MemoryStore::new();
    StoreHandle::append(&store, &mut events)
        .await
        .expect("copy durable journal");
    let artifacts = GenericArtifactReader {
        artifact: ArtifactRef::new("unused"),
        bytes: vec![],
    };
    let replay = PromptHistoryCompiler::compile_idle_with_artifacts(
        &store,
        &artifacts,
        &SessionId::new(SESSION),
        None,
        None,
    )
    .await
    .expect("compile durable history");
    assert_eq!(
        replay, requests[1].messages,
        "durable replay and live repair must agree"
    );
}

#[tokio::test]
async fn recovered_malformed_tool_keeps_safe_arguments_and_consumed_repair_allowance() {
    let mut original_script = malformed_tool_steps("bad-recovered", "{broken");
    original_script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let (_, events, _, _) = toolrepair_run(config(), original_script).await;
    let result = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .expect("durable invalid result");
    let run_id = events[0].run_id.clone().expect("run id");
    // Retain exactly the committed first response, before its repair request.
    let second_attempt = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            completed_extension(event, CACHE_REQUEST_ATTEMPT_EXTENSION_KIND).then_some(index)
        })
        .nth(1)
        .expect("second attempt");
    let mut prefix = events[..second_attempt - 1].to_vec();
    let store = Arc::new(MemoryStore::new());
    StoreHandle::append(store.as_ref(), &mut prefix)
        .await
        .expect("restore journal prefix");
    let mut replay_script = malformed_tool_steps("bad-recovered", "{broken");
    replay_script.extend(malformed_tool_steps("bad-next", "{broken"));
    replay_script.push(FakeStep::Finish {
        reason: FinishReason::EndTurn,
    });
    let provider = Arc::new(FakeProvider::new(replay_script));
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let outcome = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id,
            messages: vec![Message::user_text("repair the tool call")],
            checkpoint: RouteWaitCheckpoint {
                completed_tools: vec![haider_core::RouteWaitCompletedToolCheckpoint {
                    call_id: "bad-recovered".into(),
                    name: "inspect".into(),
                    args: serde_json::json!("{broken"),
                    result: Some(result),
                }],
                structured_events: vec![
                    haider_protocol::provider::StreamEvent::ToolCallStart {
                        call_id: "bad-recovered".into(),
                        name: "inspect".into(),
                    },
                    haider_protocol::provider::StreamEvent::ToolCallArgsDelta {
                        call_id: "bad-recovered".into(),
                        args_fragment: "{broken".into(),
                    },
                    haider_protocol::provider::StreamEvent::ToolCallEnd {
                        call_id: "bad-recovered".into(),
                    },
                ],
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("resume")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        2,
        "one replay send and one repair, no extra repair"
    );
    assert!(requests[1].messages.iter().flat_map(|message| &message.blocks).any(|block| matches!(block,
        Block::ToolCall { call_id, args, .. } if call_id == "bad-recovered" && args == &serde_json::json!({})
    )));
    handle.stop().await.expect("stop");
}

#[tokio::test]
async fn repair_allowance_survives_restart_after_a_new_request_epoch() {
    for valid_between in [false, true] {
        let mut script = malformed_tool_steps("bad-before-restart", "{broken");
        if valid_between {
            script.extend([
                FakeStep::EmitToolCall {
                    call_id: "valid-reset".into(),
                    name: "inspect".into(),
                    args: serde_json::json!({}),
                },
                FakeStep::Finish {
                    reason: FinishReason::ToolUse,
                },
            ]);
        }
        script.push(FakeStep::Finish {
            reason: FinishReason::EndTurn,
        });
        let (_, events, _, _) = toolrepair_run(config(), script).await;
        let run_id = events[0].run_id.clone().expect("run id");
        let epoch = if valid_between { 2 } else { 1 };
        let attempt = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| {
                completed_extension(event, haider_core::ROUTE_REPLAY_ATTEMPT_EXTENSION_KIND)
                    .then_some(index)
            })
            .nth(epoch - 1) // Epoch zero has no separate replay-attempt marker.
            .expect("new request epoch marker");
        let mut prefix = events[..=attempt].to_vec();
        let store = Arc::new(MemoryStore::new());
        StoreHandle::append(store.as_ref(), &mut prefix)
            .await
            .expect("restore new request prefix");
        let mut script = malformed_tool_steps("bad-after-restart", "{broken");
        script.push(FakeStep::Finish {
            reason: FinishReason::EndTurn,
        });
        let provider = Arc::new(FakeProvider::new(script));
        let handle = HarnessActor::spawn(config(), provider.clone(), store);
        let outcome = handle
            .submit_route_wait_turn(SubmitRouteWaitTurn {
                run_id,
                // The production compiler omits current-run tool results; only
                // durable repair facts can restore the allowance across epochs.
                messages: vec![Message::user_text("repair the tool call")],
                checkpoint: RouteWaitCheckpoint {
                    response_epoch: epoch as u64,
                    ..RouteWaitCheckpoint::default()
                },
            })
            .await
            .expect("resume new epoch")
            .wait()
            .await
            .expect("outcome");
        assert_eq!(
            outcome.state,
            if valid_between {
                RunState::Done
            } else {
                RunState::Errored
            }
        );
        assert_eq!(provider.requests().len(), if valid_between { 2 } else { 1 });
        handle.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn recovered_tool_name_correction_survives_a_checkpoint() {
    let cfg = toolrepair_config(&["fs_read"]);
    let (_, events, _, _) = toolrepair_run(
        cfg.clone(),
        vec![
            FakeStep::EmitToolCall {
                call_id: "name-resume".into(),
                name: "FS_READ".into(),
                args: serde_json::json!({}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    let run_id = events[0].run_id.clone().expect("run id");
    let item_id = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::ToolCall { .. },
            }) => Some(item_id),
            _ => None,
        })
        .expect("open tool item");
    let running = events
        .iter()
        .position(|event| matches!(typed(event), EventPayload::RunState(RunState::RunningTool)))
        .expect("effect boundary");
    let mut prefix = events[..running].to_vec();
    let store = Arc::new(MemoryStore::new());
    StoreHandle::append(store.as_ref(), &mut prefix)
        .await
        .expect("restore open tool");
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "name-resume".into(),
            name: "FS_READ".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        cfg,
        provider.clone(),
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id,
            messages: vec![Message::user_text("repair the tool call")],
            checkpoint: RouteWaitCheckpoint {
                tools: vec![RouteWaitToolCheckpoint {
                    item_id,
                    call_id: "name-resume".into(),
                    name: "fs_read".into(),
                    args: "{}".into(),
                }],
                structured_events: vec![
                    haider_protocol::provider::StreamEvent::ToolCallStart {
                        call_id: "name-resume".into(),
                        name: "FS_READ".into(),
                    },
                    haider_protocol::provider::StreamEvent::ToolCallArgsDelta {
                        call_id: "name-resume".into(),
                        args_fragment: "{}".into(),
                    },
                ],
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("resume")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    let events = store.events(&SessionId::new(SESSION)).await;
    let preview = events
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::ToolResult { result, .. } => Some(result.preview),
            _ => None,
        })
        .expect("durable result");
    let correction: serde_json::Value = serde_json::from_str(&preview).expect("corrected JSON");
    assert_eq!(correction["tool_name_correction"]["requested"], "FS_READ");
    assert_eq!(correction["tool_name_correction"]["resolved"], "fs_read");
    assert!(
        provider.requests()[1]
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .any(|block| matches!(block,
                Block::ToolResult { preview: live, .. } if live == &preview
            ))
    );
    handle.stop().await.expect("stop");
    task.await.expect("join");
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
async fn transport_error_after_first_event_is_typed_failure_not_input_required() {
    let (handle, store, provider) = runtime(vec![
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
    let turn = handle
        .submit_turn(SubmitTurn::new("do not duplicate output"))
        .await
        .expect("turn accepted");
    assert_eq!(turn.wait().await.expect("outcome").state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    let payloads = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .map(|event| typed(&event))
        .collect::<Vec<_>>();
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunFailed {
            code: ErrorCode::ProviderError,
            retryable: true,
            ..
        }
    )));
    assert!(!payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::InputRequired { .. })
    )));
}

#[tokio::test]
async fn link_drop_mid_stream_is_waiting_not_a_provider_fault() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "before".into(),
            },
            FakeStep::EmitNetworkUnavailable,
            FakeStep::Delay { ms: 250 },
            FakeStep::EmitNetworkRestored,
            FakeStep::EmitText {
                text: " after".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(Arc::clone(&route)),
    );
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let mut states = handle.state_receiver();
    let turn = handle
        .submit_turn(SubmitTurn::new("survive a local link drop"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                states.borrow().as_ref(),
                Some(RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                })
            ) {
                break;
            }
            states.changed().await.expect("state sender remains open");
        }
    })
    .await
    .expect("eight 250ms observations reveal the route wait");
    *route.lock().expect("route lock") = RouteStatus::Available;
    let outcome = turn.wait().await.expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 1, "link recovery never replays");

    let payloads = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .map(|event| typed(&event))
        .collect::<Vec<_>>();
    let waiting = payloads.iter().position(|payload| {
        matches!(
            payload,
            EventPayload::RunState(RunState::Waiting {
                reason: WaitReason::NetworkUnavailable
            })
        )
    });
    let restored = waiting.and_then(|waiting| {
        payloads
            .iter()
            .enumerate()
            .skip(waiting + 1)
            .find_map(|(index, payload)| {
                matches!(payload, EventPayload::RunState(RunState::Streaming)).then_some(index)
            })
    });
    assert!(
        waiting.is_some(),
        "route-down is durably attributed locally"
    );
    assert!(restored.is_some(), "route restoration returns to streaming");
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunFailed { .. }))
    );
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn network_transport_break_mid_stream_waits_resumes_same_run_without_duplicate_transcript() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "before".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::NetworkUnavailable,
                message: "connection reset".into(),
                retry_after_ms: None,
            },
            FakeStep::EmitText {
                text: "before after".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let mut states = handle.state_receiver();
    let turn = handle
        .submit_turn(SubmitTurn::new("resume after reconnect"))
        .await
        .expect("turn accepted");
    timeout(
        // Outer observation bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        async {
            loop {
                if matches!(
                    states.borrow().as_ref(),
                    Some(RunState::Waiting {
                        reason: WaitReason::NetworkUnavailable
                    })
                ) {
                    break;
                }
                states.changed().await.expect("state sender remains open");
            }
        },
    )
    .await
    .expect("route wait becomes visible");
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("resumed turn completes")
    .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "the provider view is replayed exactly"
    );

    let completed_text = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .filter_map(|event| match typed(&event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, ["before after"]);
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn network_break_after_tool_effect_replays_without_redispatch() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let call = FakeStep::EmitToolCall {
        call_id: "resume-tool".into(),
        name: "inspect".into(),
        args: serde_json::json!({"path":"src/lib.rs"}),
    };
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "before structured tool".into(),
            },
            call.clone(),
            FakeStep::Error {
                kind: ProviderErrorKind::NetworkUnavailable,
                message: "connection reset after tool completion".into(),
                retry_after_ms: None,
            },
            FakeStep::EmitText {
                text: "before structured tool".into(),
            },
            call,
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "resume-tool".into(),
            },
            FakeStep::EmitText {
                text: "tool result retained".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let mut states = handle.state_receiver();
    let turn = handle
        .submit_turn(SubmitTurn::new("resume structured output"))
        .await
        .expect("turn accepted");
    timeout(
        // Outer observation bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        async {
            loop {
                if matches!(
                    states.borrow().as_ref(),
                    Some(RunState::Waiting {
                        reason: WaitReason::NetworkUnavailable
                    })
                ) {
                    break;
                }
                states.changed().await.expect("state sender remains open");
            }
        },
    )
    .await
    .expect("route wait becomes visible");
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("resumed structured turn completes")
    .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests().len(), 3);

    let payloads = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .map(|event| typed(&event))
        .collect::<Vec<_>>();
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::ToolCall { call_id, .. },
                    ..
                }) if call_id == "resume-tool"
            ))
            .count(),
        1
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall { call_id, .. },
                    ..
                }) if call_id == "resume-tool"
            ))
            .count(),
        1
    );
    let completed_text = payloads
        .iter()
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text.to_owned_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completed_text,
        ["before structured tool", "tool result retained"]
    );
    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor task joins");
}

#[tokio::test]
async fn recovered_route_wait_does_not_reissue_until_route_returns() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "durable prefix after".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let turn = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id: RunId::new("recovered-route-wait"),
            messages: vec![Message::user_text("continue recovered run")],
            checkpoint: RouteWaitCheckpoint {
                message: Some(RouteWaitTextCheckpoint {
                    item_id: ItemId::new("recovered-message"),
                    text: "durable prefix".into(),
                }),
                reasoning: None,
                tools: Vec::new(),
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("recovered route wait accepted");

    // Proof window = 2 × the 250ms cached-route backstop period. The second
    // observation demonstrates that recovery remains parked without reissue.
    sleep(Duration::from_millis(500)).await;
    assert!(provider.requests().is_empty());
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("recovered turn completes")
    .expect("recovered outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 1);
    let completed_text = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .filter_map(|event| match typed(&event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, ["durable prefix after"]);
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn recovered_route_wait_rebuilds_completed_and_open_text_in_event_order() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText { text: "A".into() },
            FakeStep::EmitProviderOpaque {
                provider: "fake".into(),
                data: serde_json::json!({"state":"between"}),
            },
            FakeStep::EmitText { text: "B".into() },
            FakeStep::EmitText { text: "C".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let turn = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id: RunId::new("recovered-mixed-text-route-wait"),
            messages: vec![Message::user_text("continue mixed response")],
            checkpoint: RouteWaitCheckpoint {
                message: Some(RouteWaitTextCheckpoint {
                    item_id: ItemId::new("recovered-open-message"),
                    text: "B".into(),
                }),
                structured_events: vec![
                    haider_protocol::provider::StreamEvent::TextDelta { text: "A".into() },
                    haider_protocol::provider::StreamEvent::ProviderOpaque {
                        provider: "fake".into(),
                        data: serde_json::json!({"state":"between"}).into(),
                    },
                    haider_protocol::provider::StreamEvent::TextDelta { text: "B".into() },
                ],
                response_epoch: 3,
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("mixed response recovery accepted");

    // Proof window = 2 × the 250ms cached-route backstop period.
    sleep(Duration::from_millis(500)).await;
    assert!(provider.requests().is_empty());
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("mixed response recovery completes")
    .expect("mixed response outcome");
    assert_eq!(outcome.state, RunState::Done, "outcome: {outcome:?}");
    assert_eq!(provider.requests().len(), 1);
    let completed_text = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .filter_map(|event| match typed(&event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, ["BC"]);
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn recovered_route_wait_restores_partial_tool_without_duplicate_dispatch() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitToolCallStart {
                call_id: "recovered-tool".into(),
                name: "inspect".into(),
            },
            FakeStep::EmitToolArgsDelta {
                call_id: "recovered-tool".into(),
                fragment: "{\"path\":\"".into(),
            },
            FakeStep::EmitToolArgsDelta {
                call_id: "recovered-tool".into(),
                fragment: "src".into(),
            },
            FakeStep::EmitToolArgsDelta {
                call_id: "recovered-tool".into(),
                fragment: r#"/lib.rs"}"#.into(),
            },
            FakeStep::EmitToolCallEnd {
                call_id: "recovered-tool".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "recovered-tool".into(),
            },
            FakeStep::EmitText {
                text: "recovered tool finished".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let turn = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id: RunId::new("recovered-tool-route-wait"),
            messages: vec![Message::user_text("finish recovered tool")],
            checkpoint: RouteWaitCheckpoint {
                message: None,
                reasoning: None,
                tools: vec![RouteWaitToolCheckpoint {
                    item_id: ItemId::new("recovered-tool-item"),
                    call_id: "recovered-tool".into(),
                    name: "inspect".into(),
                    args: r#"{"path":"src"#.into(),
                }],
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("recovered route wait accepted");

    // Proof window = 2 × the 250ms cached-route backstop period. Recovery
    // must remain parked without opening or dispatching the provider call.
    sleep(Duration::from_millis(500)).await;
    assert!(provider.requests().is_empty());
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 0);
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("recovered tool turn completes")
    .expect("recovered tool outcome");
    assert_eq!(outcome.state, RunState::Done, "outcome: {outcome:?}");
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.requests().len(), 2);
    let completed_text = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .filter_map(|event| match typed(&event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, ["recovered tool finished"]);
    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor task joins");
}

#[tokio::test]
async fn recovered_route_wait_restores_completed_effect_without_redispatch() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "completed before effect".into(),
            },
            FakeStep::EmitToolCall {
                call_id: "completed-before-crash".into(),
                name: "inspect".into(),
                args: serde_json::json!({"path":"src/lib.rs"}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::ExpectToolResult {
                call_id: "completed-before-crash".into(),
            },
            FakeStep::EmitText {
                text: "effect was not repeated".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ])
        .with_route_status(route.clone()),
    );
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let call_id = "completed-before-crash".to_owned();
    let name = "inspect".to_owned();
    let args = serde_json::json!({"path":"src/lib.rs"});
    let turn = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id: RunId::new("recovered-completed-effect"),
            messages: vec![Message::user_text("continue after completed effect")],
            checkpoint: RouteWaitCheckpoint {
                completed_tools: vec![haider_core::RouteWaitCompletedToolCheckpoint {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    result: Some(BoundedResult {
                        preview: "done once".into(),
                        truncated: false,
                        truncation: None,
                        effects: Vec::new(),
                        data: None,
                        artifact: None,
                        images: Vec::new(),
                        cursor: None,
                        status: haider_protocol::tool::ToolResultStatus::Completed,
                        reason: None,
                        presentation: None,
                    }),
                }],
                structured_events: vec![
                    haider_protocol::provider::StreamEvent::TextDelta {
                        text: "completed before effect".into(),
                    },
                    haider_protocol::provider::StreamEvent::ToolCallStart {
                        call_id: call_id.clone(),
                        name,
                    },
                    haider_protocol::provider::StreamEvent::ToolCallArgsDelta {
                        call_id: call_id.clone(),
                        args_fragment: args.to_string(),
                    },
                    haider_protocol::provider::StreamEvent::ToolCallEnd { call_id },
                ],
                response_epoch: 7,
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("completed-effect recovery accepted");

    // Proof window = 2 × the 250ms cached-route backstop period.
    sleep(Duration::from_millis(500)).await;
    assert!(provider.requests().is_empty());
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 0);
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(
        // Completion bound = 8 × the 250ms route backstop period.
        Duration::from_secs(2),
        turn.wait(),
    )
    .await
    .expect("completed-effect recovery finishes")
    .expect("completed-effect outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.requests().len(), 2);
    let completed_text = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .filter_map(|event| match typed(&event) {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { text },
                ..
            }) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_text, ["effect was not repeated"]);
    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor task joins");
}

#[tokio::test]
async fn route_wait_deadline_keeps_wait_fact_and_never_reissues() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(
        FakeProvider::new(vec![FakeStep::Error {
            kind: ProviderErrorKind::NetworkUnavailable,
            message: "dns unavailable".into(),
            retry_after_ms: None,
        }])
        .with_route_status(route),
    );
    let store = Arc::new(MemoryStore::new());
    let mut cfg = config();
    cfg.provider_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(1_200));
    let handle = HarnessActor::spawn(cfg, provider.clone(), store.clone());
    let outcome = timeout(
        // Outer bound = 1.2s run deadline + 4 × 250ms route observations.
        Duration::from_millis(2_200),
        handle
            .submit_turn(SubmitTurn::new("deadline while offline"))
            .await
            .expect("turn accepted")
            .wait(),
    )
    .await
    .expect("deadline is bounded")
    .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(ErrorCode::ProviderTimeout)
    );
    assert_eq!(provider.requests().len(), 1);
    let payloads = store
        .events(&SessionId::new(SESSION))
        .await
        .into_iter()
        .map(|event| typed(&event))
        .collect::<Vec<_>>();
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::RunState(RunState::Waiting {
            reason: WaitReason::NetworkUnavailable
        })
    )));
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(payload, EventPayload::RunFailed { .. }))
            .count(),
        1
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|payload| matches!(payload, EventPayload::RunState(RunState::Errored)))
            .count(),
        1,
        "the run owns exactly one terminal state"
    );
    handle.stop().await.expect("actor stops");
}

/// MUTATION CHECK: remove/invert the negative RouteStatus gate. A live route
/// would then be misreported as `WaitingForRoute` before the retry whose open
/// future never completes. The outer bound is the 3s provider deadline plus
/// four 250ms route-observation periods for terminal delivery.
#[tokio::test(start_paused = true)]
async fn live_route_failure_retries_to_never_opening_provider_without_route_wait() {
    let route = Arc::new(Mutex::new(RouteStatus::Available));
    let provider = Arc::new(NetworkFailureThenNeverOpensProvider::new(Arc::clone(
        &route,
    )));
    let store = Arc::new(MemoryStore::new());
    let mut cfg = config();
    let provider_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    cfg.provider_deadline = Some(provider_deadline);
    let handle = HarnessActor::spawn(cfg, provider.clone(), store.clone());
    let started = tokio::time::Instant::now();
    let outcome = timeout(
        Duration::from_secs(4),
        handle
            .submit_turn(SubmitTurn::new("live route must not park"))
            .await
            .expect("turn accepted")
            .wait(),
    )
    .await
    .expect("provider deadline plus route-observation allowance is bounded")
    .expect("turn outcome");

    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(ErrorCode::ProviderTimeout)
    );
    assert!(started.elapsed() < provider_deadline.duration_since(started));
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    assert!(
        !store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .any(|event| matches!(
                typed(event),
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                })
            ))
    );
    handle.stop().await.expect("actor stops");
}

/// MUTATION CHECK: invert the same negative-only gate. A confirmed down route
/// would skip the wait, while the live-route sibling above would enter it, so
/// both tests cannot pass under either polarity mutation. The completion bound
/// is the 4s provider deadline plus four 250ms route-observation periods.
#[tokio::test(start_paused = true)]
async fn actually_down_route_waits_once_then_live_retry_never_opens_and_terminalizes() {
    let route = Arc::new(Mutex::new(RouteStatus::Unavailable));
    let provider = Arc::new(NetworkFailureThenNeverOpensProvider::new(Arc::clone(
        &route,
    )));
    let store = Arc::new(MemoryStore::new());
    let mut cfg = config();
    cfg.provider_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(4));
    let handle = HarnessActor::spawn(cfg, provider.clone(), store.clone());
    let mut states = handle.state_receiver();
    let turn = handle
        .submit_turn(SubmitTurn::new("wait only while route is down"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                states.borrow().as_ref(),
                Some(RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                })
            ) {
                break;
            }
            states.changed().await.expect("state sender remains open");
        }
    })
    .await
    .expect("eight 250ms observations reveal the route wait");
    assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
    *route.lock().expect("route lock") = RouteStatus::Available;

    let outcome = timeout(Duration::from_secs(5), turn.wait())
        .await
        .expect("provider deadline plus four route observations is bounded")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.as_ref().map(|error| error.code),
        Some(ErrorCode::ProviderTimeout)
    );
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .filter(|event| matches!(
                typed(event),
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                })
            ))
            .count(),
        1,
        "a repeated failure on the restored route must not repark"
    );
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn provider_http_5xx_never_enters_route_wait() {
    let mut provider_error =
        ProviderError::new(ProviderErrorKind::Transport, "provider returned 503")
            .with_http_metadata(503, Some("request-503"));
    provider_error.retryable = false;
    let provider = Arc::new(ImmediateErrorProvider {
        error: provider_error,
    });
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config(), provider, store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("do not route-wait on HTTP"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert!(
        !store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .any(|event| matches!(
                typed(event),
                EventPayload::RunState(RunState::Waiting {
                    reason: WaitReason::NetworkUnavailable
                })
            ))
    );
    handle.stop().await.expect("actor stops");
}

#[tokio::test]
async fn explicit_hosted_web_rejection_falls_back_locally_once_in_the_same_turn() {
    let rejected = ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "hosted web search rejected",
    )
    .with_presentation(haider_protocol::error::ErrorPresentation::new(
        "provider-web-tool-rejected",
        "Provider web tool unavailable",
        "use the local equivalent",
        haider_protocol::error::ErrorScope::Tool,
        [haider_protocol::error::ErrorAction::Retry],
    ));
    let primary = Arc::new(CountingOpeningErrorProvider::new(rejected));
    let fallback = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let resolver = Arc::new(CapabilityFallbackResolver {
        calls: AtomicUsize::new(0),
        provider: fallback.clone(),
    });
    let store = Arc::new(MemoryStore::new());
    let mut cfg = config();
    cfg.usage_account = Some(CredentialAlias::new("web-account"));
    cfg.provider_attempt_resolver = Some(resolver.clone());
    cfg.provider_tool_fallback_tools = vec![haider_provider::ToolDefinition {
        name: "web_fetch".into(),
        description: "local fallback".into(),
        input_schema: serde_json::json!({"type":"object"}),
    }];
    let handle = HarnessActor::spawn(cfg, primary.clone(), store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("search"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(primary.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fallback.requests().len(), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fallback.requests()[0].tools[0].name, "web_fetch");
    assert_eq!(
        store
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .filter(|event| completed_extension(event, "provider_tool_fallback"))
            .count(),
        1,
        "the same-turn fallback is visibly labeled once"
    );
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
        cache_account: Mutex::new(None),
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
    durable_config.cache_reuse_gap_ms = Some(999_999);
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
    assert_eq!(
        alternate
            .cache_account
            .lock()
            .expect("cache account lock")
            .clone(),
        Some(("account-b".to_owned(), None)),
        "the rotated attempt must leave the old account cache domain and its measured gap"
    );
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

    {
        let partial = Arc::new(FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "visible".into(),
            },
            FakeStep::Error {
                kind: ProviderErrorKind::RateLimited,
                message: "after text".into(),
                retry_after_ms: Some(0),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ]));
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
        let turn = partial_handle
            .submit_turn(SubmitTurn::new("surface partial output honestly"))
            .await
            .expect("turn accepted");
        let mut state = partial_handle.state_receiver();
        let parked = state
            .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
            .await
            .expect("partial recovery remains answerable")
            .clone();
        let RunState::InputRequired { menu } = parked.expect("input state") else {
            panic!("expected partial-stream menu");
        };
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
        partial_handle
            .answer_menu(MenuAnswer {
                menu,
                option_key: Some("retry_fresh".into()),
                option_index: 1,
                value: None,
                via: AnswerVia::Rpc,
            })
            .await
            .expect("retry-fresh answer");
        assert_eq!(turn.wait().await.expect("outcome").state, RunState::Done);
        assert_eq!(partial.requests().len(), 2);
        assert_eq!(partial_resolver.calls.load(Ordering::SeqCst), 0);
    }

    for script in [
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
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
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
                normalized: None,
                scope: None,
                cache_cost: None,
                request: None,
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

/// Rebind factory-time rotation obeys the same durable-before-POST law as a
/// turn's initial account resolution, including publishing pending Thinking.
#[tokio::test]
async fn provider_rebind_initial_rotation_is_durable_before_target_request() {
    let store = Arc::new(MemoryStore::new());
    let target = Arc::new(RotationAwareFinishProvider {
        store: store.clone(),
        requests: AtomicUsize::new(0),
        saw_rotation_before_request: AtomicBool::new(false),
        cache_account: Mutex::new(None),
    });
    let rotation = RotationEvent {
        provider: "fake".into(),
        from: CredentialAlias::new("rebind-a"),
        to: CredentialAlias::new("rebind-b"),
        cause: RotationCause::RateLimit,
    };
    let original = Arc::new(FakeProvider::new(Vec::new()));
    let mut cfg = config();
    cfg.usage_account = Some(rotation.from.clone());
    cfg.provider_rebind_resolver = Some(Arc::new(OneRebindResolver(Mutex::new(Some(
        ProviderRebindTarget {
            provider: target.clone(),
            provider_name: "fake".into(),
            account: Some(rotation.to.clone()),
            context_window: None,
            cached_input_is_subset: false,
            provider_request_state: Default::default(),
            auth_scope: "api_key".into(),
            attempt_resolver: None,
            route_epoch: "factory-rotated-rebind".into(),
            initial_rotation: Some(rotation.clone()),
            // The Rotation itself also spends the budget; these two facts
            // need not be redundant for every resolver implementation.
            rotation_budget_consumed: false,
        },
    )))));
    let handle = HarnessActor::spawn(cfg, original.clone(), store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("rebind factory rotated"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(original.requests().is_empty());
    assert_eq!(target.requests.load(Ordering::SeqCst), 1);
    assert!(target.saw_rotation_before_request.load(Ordering::SeqCst));
    assert_eq!(
        *target.cache_account.lock().expect("cache scope"),
        Some(("rebind-b".into(), None))
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    let rotations: Vec<_> = events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::Rotation(value) => Some((event.seq, value)),
            _ => None,
        })
        .collect();
    assert_eq!(rotations.len(), 1);
    assert_eq!(rotations[0].1, rotation);
    assert!(
        events.iter().any(|event| event.seq < rotations[0].0
            && matches!(typed(event), EventPayload::RunState(RunState::Thinking))),
        "pending Thinking must be committed before the rebind Rotation"
    );
    handle.stop().await.expect("actor stops");
}

/// The target's consumed flag, its factory rotation, and the turn's prior
/// consumed flag independently forbid another automatic rotation on auth
/// failure. A fresh-budget control proves the target resolver is reachable.
#[tokio::test]
async fn provider_rebind_cannot_refund_or_drop_consumed_rotation_budget() {
    for (prior_consumed, target_consumed, with_rotation) in [
        (false, true, false),
        (false, false, true),
        (true, false, false),
        (false, false, false),
    ] {
        let store = Arc::new(MemoryStore::new());
        let failed_target = Arc::new(CountingOpeningErrorProvider::inspecting(
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "rebound account rejects auth",
            ),
            store.clone(),
        ));
        let alternate = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
            reason: FinishReason::EndTurn,
        }]));
        let resolver = Arc::new(ScriptedRotationResolver {
            calls: AtomicUsize::new(0),
            first: alternate.clone(),
            first_alias: CredentialAlias::new("rebind-c"),
            second: alternate.clone(),
            second_alias: CredentialAlias::new("rebind-d"),
        });
        let rotation = with_rotation.then(|| RotationEvent {
            provider: "fake".into(),
            from: CredentialAlias::new("rebind-a"),
            to: CredentialAlias::new("rebind-b"),
            cause: RotationCause::Error,
        });
        let mut cfg = config();
        cfg.usage_account = Some(CredentialAlias::new("rebind-a"));
        cfg.rotation_budget_consumed = prior_consumed;
        cfg.provider_rebind_resolver = Some(Arc::new(OneRebindResolver(Mutex::new(Some(
            ProviderRebindTarget {
                provider: failed_target.clone(),
                provider_name: "fake".into(),
                account: Some(CredentialAlias::new("rebind-b")),
                context_window: None,
                cached_input_is_subset: false,
                provider_request_state: Default::default(),
                auth_scope: "api_key".into(),
                attempt_resolver: Some(resolver.clone()),
                route_epoch: "rebind-budget".into(),
                initial_rotation: rotation,
                rotation_budget_consumed: target_consumed,
            },
        )))));
        let handle =
            HarnessActor::spawn(cfg, Arc::new(FakeProvider::new(Vec::new())), store.clone());
        let outcome = handle
            .submit_turn(SubmitTurn::new("rebind auth failure"))
            .await
            .expect("turn accepted")
            .wait()
            .await
            .expect("turn outcome");
        let consumed = prior_consumed || target_consumed || with_rotation;
        assert_eq!(
            outcome.state,
            if consumed {
                RunState::Errored
            } else {
                RunState::Done
            },
            "prior={prior_consumed} target={target_consumed} rotation={with_rotation}"
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            usize::from(!consumed)
        );
        assert_eq!(alternate.requests().len(), usize::from(!consumed));
        assert_eq!(failed_target.requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            failed_target
                .saw_rotation_before_request
                .load(Ordering::SeqCst),
            with_rotation
        );
        assert_eq!(
            store
                .events(&SessionId::new(SESSION))
                .await
                .iter()
                .filter(|event| matches!(typed(event), EventPayload::Rotation(_)))
                .count(),
            usize::from(with_rotation) + usize::from(!consumed)
        );
        handle.stop().await.expect("actor stops");
    }
}

struct OneRebindResolver(Mutex<Option<ProviderRebindTarget>>);

impl std::fmt::Debug for OneRebindResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OneRebindResolver")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderRebindResolver for OneRebindResolver {
    async fn refresh(
        &self,
        _model: &str,
        _reasoning: &str,
    ) -> Result<Option<ProviderRebindTarget>, HaiderError> {
        Ok(self.0.lock().expect("one rebind snapshot").take())
    }
}

/// H2: a resolver that decides `Retry` (credential refresh) on EVERY 401 must
/// not loop forever. The refresh is budgeted under `MAX_API_RETRIES`, so a
/// persistently-failing 401 falls through to `Errored` within a bound instead
/// of spinning. The resolver carries a safety hatch (returns `Stop` after 50
/// calls) so a REGRESSED build still terminates the test — the assertion on the
/// consulted count is what distinguishes fixed (≤ the cap) from broken (50).
#[tokio::test]
async fn persistent_401_refresh_terminates_in_errored_within_a_bound() {
    let auth_error = || ProviderError::new(ProviderErrorKind::Authentication, "401 unauthorized");
    let provider = Arc::new(CountingOpeningErrorProvider::new(auth_error()));
    let resolver = Arc::new(RefreshingAttemptResolver {
        calls: AtomicUsize::new(0),
        provider: provider.clone(),
        escape_after: 50,
    });
    let mut cfg = config();
    cfg.usage_account = Some(CredentialAlias::new("acct-401"));
    cfg.provider_attempt_resolver = Some(resolver.clone());
    let handle = HarnessActor::spawn(cfg, provider.clone(), Arc::new(MemoryStore::new()));
    let outcome = handle
        .submit_turn(SubmitTurn::new("keep failing 401"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(
        outcome.state,
        RunState::Errored,
        "a persistently-failing 401 must terminate, not loop"
    );
    let calls = resolver.calls.load(Ordering::SeqCst);
    // MAX_API_RETRIES is 10; the refresh budget caps the resolver consultations
    // at that. An unbudgeted (regressed) build hits the 50-call safety hatch.
    assert!(
        (1..=10).contains(&calls),
        "the refresh must be budgeted under MAX_API_RETRIES (consulted {calls} times)"
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
        // M4: the pre-first-event backoff now commits the visible `Retrying`
        // state (with the attempt counter) in place of a bare `Waiting`.
        if matches!(
            typed(&envelope),
            EventPayload::RunState(RunState::Retrying { .. })
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedDispatch {
    call_id: String,
    provider_requests_seen: usize,
    subturn_seen: bool,
}

struct BoundaryRecordingDispatcher {
    provider: Arc<FakeProvider>,
    records: Mutex<Vec<RecordedDispatch>>,
    subturn_text: String,
}

#[async_trait]
impl ToolDispatcher for BoundaryRecordingDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        let requests = self.provider.requests();
        let subturn_seen = requests.last().is_some_and(|request| {
            request.messages.iter().any(|message| {
                message.blocks.iter().any(
                    |block| matches!(block, Block::Text { text } if text == &self.subturn_text),
                )
            })
        });
        self.records
            .lock()
            .expect("dispatch records lock")
            .push(RecordedDispatch {
                call_id: call_id.to_owned(),
                provider_requests_seen: requests.len(),
                subturn_seen,
            });
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "done".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

/// ST2/ST3 LAW. MUTATION CHECK: call `complete_tool` before consuming
/// `pending_subturns`, or omit the user-message append. Expected runtime
/// failure: `pending-before-subturn` reaches the dispatcher, the dispatcher
/// sees only one provider request, or request two lacks the injected text.
#[tokio::test]
async fn subturn_holds_the_pending_tool_and_reprompts_before_dispatch() {
    let subturn_text = "use the narrow fixture before you inspect";
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitText {
            text: "I will inspect the broad fixture first.".into(),
        },
        FakeStep::Delay { ms: 80 },
        FakeStep::EmitToolCall {
            call_id: "pending-before-subturn".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"broad.rs"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::EmitToolCall {
            call_id: "revised-after-subturn".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"narrow.rs"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "revised-after-subturn".into(),
        },
        FakeStep::EmitText {
            text: "The narrow fixture is correct.".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let dispatcher = Arc::new(BoundaryRecordingDispatcher {
        provider: provider.clone(),
        records: Mutex::new(Vec::new()),
        subturn_text: subturn_text.into(),
    });
    let store = Arc::new(MemoryStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("inspect the fixture"))
        .await
        .expect("turn accepted");

    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("text event");
            if matches!(
                typed(&event),
                EventPayload::Item(ItemEvent::Delta {
                    delta: haider_protocol::item::ItemDelta::Text { ref text },
                    ..
                }) if text == "I will inspect the broad fixture first."
            ) {
                break;
            }
        }
    })
    .await
    .expect("current response text arrives first");
    handle
        .subturn(subturn_text)
        .expect("subturn accepted without blocking");

    let outcome = timeout(Duration::from_secs(2), turn.wait())
        .await
        .expect("subturn turn completes")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    let records = dispatcher.records.lock().expect("dispatch records").clone();
    assert_eq!(
        records,
        vec![RecordedDispatch {
            call_id: "revised-after-subturn".into(),
            provider_requests_seen: 2,
            subturn_seen: true,
        }],
        "the original call must be held and only the post-subturn call may dispatch"
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let second = &requests[1].messages;
    let current_text_index = second
        .iter()
        .position(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, Block::Text { text } if text == "I will inspect the broad fixture first.")
            })
        })
        .expect("current response text is retained");
    let subturn_index = second
        .iter()
        .position(|message| {
            message
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if text == subturn_text))
        })
        .expect("subturn text is injected");
    assert!(
        current_text_index < subturn_index,
        "the subturn must not overtake current response text"
    );
    let events = store.events(&SessionId::new(SESSION)).await;
    assert!(events.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ToolCall { call_id, status: ToolStatus::Pending, .. },
                ..
            }) if call_id == "pending-before-subturn"
        )
    }));

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}

/// ST4 LAW. MUTATION CHECK: clear `pending_subturns` at a pure-text Finish.
/// Expected runtime failure: there is only one provider request or the second
/// request does not contain the held input.
#[tokio::test]
async fn subturn_without_a_tool_degrades_to_turn_end_delivery() {
    let subturn_text = "add the restart caveat";
    let (handle, _store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "Here is the initial answer.".into(),
        },
        FakeStep::Delay { ms: 80 },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
        FakeStep::EmitText {
            text: "The restart caveat is included.".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);
    let mut subscriber = handle.subscribe();
    let turn = handle
        .submit_turn(SubmitTurn::new("answer directly"))
        .await
        .expect("turn accepted");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = subscriber.recv().await.expect("text event");
            if matches!(
                typed(&event),
                EventPayload::Item(ItemEvent::Delta {
                    delta: haider_protocol::item::ItemDelta::Text { ref text },
                    ..
                }) if text == "Here is the initial answer."
            ) {
                break;
            }
        }
    })
    .await
    .expect("initial text arrives");
    handle.subturn(subturn_text).expect("subturn accepted");
    let outcome = timeout(Duration::from_secs(2), turn.wait())
        .await
        .expect("degraded subturn completes")
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text == subturn_text))
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
    // Each request has a cache diagnostic, request-budget status, assistant
    // item and paired Finish marker. Eight unique IDs prove none are reused
    // across restart, including the newly allocated terminal markers.
    assert_eq!(item_ids.len(), 8);
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
        // Budget status adds Started + Completed to the former 8 events;
        // the paired provider Finish marker adds two more: 8 + 2 + 2.
        12
    );
    let tail = StoreHandle::read(store.as_ref(), &SessionId::new(SESSION), 3, 10)
        .await
        .expect("read");
    assert_eq!(
        tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![4, 5, 6, 7, 8, 9, 10, 11, 12]
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
    reject_tool_settlement: bool,
    reject_repair_reset: bool,
    reject_repair_read: bool,
    rejected_tool_settlement: AtomicBool,
}

impl BatchRecordingStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            batches: Mutex::new(Vec::new()),
            reject_tool_settlement: false,
            reject_repair_reset: false,
            reject_repair_read: false,
            rejected_tool_settlement: AtomicBool::new(false),
        }
    }

    fn rejecting_tool_settlement() -> Self {
        Self {
            reject_tool_settlement: true,
            ..Self::new()
        }
    }

    fn batches(&self) -> Vec<Vec<EventPayload>> {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn rejected_tool_settlement(&self) -> bool {
        self.rejected_tool_settlement.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl StoreHandle for BatchRecordingStore {
    async fn append(&self, envelopes: &mut [RawEnvelope]) -> Result<CommittedRange, HaiderError> {
        let batch = envelopes
            .iter()
            .map(|envelope| {
                serde_json::from_value(envelope.payload.clone().into()).map_err(|error| {
                    HaiderError::new(
                        ErrorCode::Internal,
                        format!("recorded payload did not decode: {error}"),
                        false,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if self.reject_repair_reset && batch.iter().any(|payload| matches!(payload,
            EventPayload::Item(ItemEvent::Completed { item: TurnItem::Extension { kind, .. }, .. })
                if kind == "tool_call_repair_reset"
        )) {
            return Err(HaiderError::new(ErrorCode::Internal, "injected repair reset failure", false));
        }
        let reject_tool_settlement = self.reject_tool_settlement
            && matches!(
                batch.as_slice(),
                [
                    EventPayload::ToolResult { .. },
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::ToolCall { .. },
                        ..
                    }),
                    EventPayload::NodeCommitted(_),
                    EventPayload::RunState(RunState::Streaming)
                ]
            );
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(batch);
        if reject_tool_settlement {
            self.rejected_tool_settlement.store(true, Ordering::Relaxed);
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "injected atomic tool-settlement rejection",
                false,
            ));
        }
        self.inner.append(envelopes).await
    }

    async fn read(
        &self,
        session_id: &SessionId,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<RawEnvelope>, HaiderError> {
        if self.reject_repair_read {
            return Err(HaiderError::new(
                ErrorCode::Internal,
                "injected repair recovery read failure",
                false,
            ));
        }
        self.inner.read(session_id, since_seq, limit).await
    }

    async fn latest_seq(&self, session_id: &SessionId) -> Result<u64, HaiderError> {
        self.inner.latest_seq(session_id).await
    }

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        self.inner.branch_lineage(session_id, branch_id).await
    }
}

struct DurableRunningToolDispatcher {
    store: Arc<BatchRecordingStore>,
    observed: AtomicBool,
}

struct TruncatedToolDispatcher;

#[async_trait]
impl ToolDispatcher for TruncatedToolDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: format!("HEAD:{}:TAIL_FAILURE", "x".repeat(10_000)),
            truncated: true,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

#[async_trait]
impl ToolDispatcher for DurableRunningToolDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        let running_tool_is_durable = self
            .store
            .inner
            .events(&SessionId::new(SESSION))
            .await
            .iter()
            .any(|event| matches!(typed(event), EventPayload::RunState(RunState::RunningTool)));
        self.observed
            .store(running_tool_is_durable, Ordering::Relaxed);
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "atomic result".into(),
            truncated: false,
            truncation: None,
            effects: Vec::new(),
            data: None,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: haider_protocol::tool::ToolResultStatus::Completed,
            reason: None,
            presentation: None,
        }))
    }
}

/// MUTATION CHECK: restore the separate `Thinking` and cache-attempt appends.
/// Expected failure: no batch contains the exact five-event request boundary
/// (Thinking + two cache events + two visible budget-progress events).
#[tokio::test]
async fn request_start_batches_thinking_with_cache_attempt_in_event_order() {
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::Finish {
        reason: FinishReason::EndTurn,
    }]));
    let store = Arc::new(BatchRecordingStore::new());
    let handle = HarnessActor::spawn(config(), provider, store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("batch request start"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let request_batch = store
        .batches()
        .into_iter()
        .find(|batch| {
            batch.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::Item(ItemEvent::Completed {
                        item: TurnItem::Extension { kind, .. },
                        ..
                    }) if kind == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
                )
            })
        })
        .expect("cache-attempt batch");
    assert!(matches!(
        request_batch.as_slice(),
        [
            EventPayload::RunState(RunState::Thinking),
            EventPayload::Item(ItemEvent::Started {
                item: TurnItem::Extension { kind: started, .. },
                ..
            }),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind: completed, .. },
                ..
            }),
            EventPayload::Item(ItemEvent::Started {
                item: TurnItem::Extension { kind: budget_started, .. },
                ..
            }),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Extension { kind: budget_completed, .. },
                ..
            }),
        ] if started == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
            && completed == CACHE_REQUEST_ATTEMPT_EXTENSION_KIND
            && budget_started == haider_protocol::request_budget::PROVIDER_REQUEST_BUDGET_EXTENSION_KIND
            && budget_completed == haider_protocol::request_budget::PROVIDER_REQUEST_BUDGET_EXTENSION_KIND
    ));
}

/// MUTATION CHECK: split exact footprint publication from usage again.
/// Expected failure: the usage batch becomes a singleton.
#[tokio::test]
async fn usage_batches_context_footprint_before_usage() {
    let usage = Usage {
        input: 13,
        output: 5,
        reasoning: 2,
        cached: 3,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    };
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitUsage { usage },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(BatchRecordingStore::new());
    let mut runtime_config = config();
    runtime_config.context_compaction_v1 = true;
    let handle = HarnessActor::spawn(runtime_config, provider, store.clone());

    let outcome = handle
        .submit_turn(SubmitTurn::new("batch exact usage"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let usage_batch = store
        .batches()
        .into_iter()
        .find(|batch| {
            batch
                .iter()
                .any(|payload| matches!(payload, EventPayload::Usage(_)))
        })
        .expect("usage batch");
    assert!(matches!(
        usage_batch.as_slice(),
        [
            EventPayload::Item(ItemEvent::Started {
                item_id: started_id,
                item: started_item @ TurnItem::Extension { .. },
                ..
            }),
            EventPayload::Item(ItemEvent::Completed {
                item_id: completed_id,
                item: completed_item @ TurnItem::Extension { .. },
                ..
            }),
            EventPayload::Usage(_)
        ] if started_id == completed_id
            && ContextFootprint::from_extension_item(started_item).is_some()
            && ContextFootprint::from_extension_item(completed_item).is_some()
    ));
}

/// MUTATION CHECK: move `RunningTool` after dispatcher execution, or split the
/// result/completion/node/Streaming group. Expected failure: the dispatcher
/// cannot read the durable state or the settlement batch shape changes.
#[tokio::test]
async fn completed_tool_settlement_is_atomic_after_durable_running_tool() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "atomic-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "atomic-tool".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(BatchRecordingStore::new());
    let dispatcher = Arc::new(DurableRunningToolDispatcher {
        store: store.clone(),
        observed: AtomicBool::new(false),
    });
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider,
        store.clone(),
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());

    let outcome = handle
        .submit_turn(SubmitTurn::new("run atomically"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(dispatcher.observed.load(Ordering::Relaxed));

    let settlement = store
        .batches()
        .into_iter()
        .find(|batch| {
            batch.iter().any(|payload| {
                matches!(
                    payload,
                    EventPayload::ToolResult { call_id, .. } if call_id == "atomic-tool"
                )
            })
        })
        .expect("tool settlement batch");
    assert!(matches!(
        settlement.as_slice(),
        [
            EventPayload::ToolResult { call_id: result, .. },
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ToolCall {
                    call_id: completed,
                    status: ToolStatus::Completed,
                    ..
                },
                ..
            }),
            EventPayload::NodeCommitted(_),
            EventPayload::RunState(RunState::Streaming)
        ] if result == "atomic-tool" && completed == "atomic-tool"
    ));

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}

#[tokio::test]
async fn output_savings_is_one_atomic_child_event_and_replay_uses_only_the_bounded_projection() {
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "accounted-tool".into(),
            name: "inspect".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "accounted-tool".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(BatchRecordingStore::new());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider.clone(),
        store.clone(),
        Some(Arc::new(TruncatedToolDispatcher)),
    );
    let actor_task = tokio::spawn(actor.run());

    let outcome = handle
        .submit_turn(SubmitTurn::new("account one bounded output"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);

    let settlement = store
        .batches()
        .into_iter()
        .find(|batch| {
            batch.iter().any(|payload| {
                matches!(payload, EventPayload::ToolResult { call_id, .. } if call_id == "accounted-tool")
            })
        })
        .expect("tool settlement batch");
    let [
        EventPayload::ToolResult { result, .. },
        EventPayload::Item(ItemEvent::Completed {
            item_id: tool_item_id,
            item: TurnItem::ToolCall { .. },
        }),
        EventPayload::Item(ItemEvent::Started {
            item_id: savings_started_id,
            item: savings_started,
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id: savings_completed_id,
            item: savings_completed,
        }),
        EventPayload::NodeCommitted(_),
        EventPayload::RunState(RunState::Streaming),
    ] = settlement.as_slice()
    else {
        panic!("truncated tool settlement must atomically carry one savings extension")
    };
    assert!(result.preview.ends_with("TAIL_FAILURE"));
    assert!(!result.preview.contains("haider_elision_v1"));
    assert_eq!(savings_started_id, savings_completed_id);
    assert_eq!(savings_started, savings_completed);
    let savings = ContextSavingsEvent::from_extension_item(savings_completed)
        .expect("typed output savings extension");
    let output = savings.tool_output().expect("output-level child");
    assert_eq!(
        output.source_item_id.as_deref(),
        Some(tool_item_id.as_str())
    );
    assert!(savings.estimated_tokens_saved > 0);
    assert_eq!(savings.session_operation_count, 1);
    assert_eq!(
        savings.session_cumulative_estimated_tokens_saved,
        savings.estimated_tokens_saved
    );

    let requests = provider.requests();
    let projected_result = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .find_map(|block| match block {
            Block::ToolResult { preview, .. } => Some(preview),
            _ => None,
        })
        .expect("second request carries the tool result");
    assert!(projected_result.contains("haider_elision_v1"));
    assert!(projected_result.ends_with("TAIL_FAILURE"));
    assert!(!projected_result.contains(&"x".repeat(9_000)));

    let recovered =
        PromptHistoryCompiler::latest_context_economy(store.as_ref(), &SessionId::new(SESSION))
            .await
            .expect("savings journal reduces")
            .expect("savings coordinate exists");
    assert_eq!(recovered.last_output_event.as_ref(), Some(&savings));

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
}

#[tokio::test]
async fn repair_reset_store_failure_closes_pending_tools_without_dispatch() {
    let store = Arc::new(BatchRecordingStore {
        reject_repair_reset: true,
        ..BatchRecordingStore::new()
    });
    let mut script = malformed_tool_steps("invalid-before-reset", "{broken");
    script.extend([
        FakeStep::EmitToolCall {
            call_id: "valid-reset-fails".into(),
            name: "inspect".into(),
            args: serde_json::json!({}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
    ]);
    let provider = Arc::new(FakeProvider::new(script));
    let dispatcher = Arc::new(CountingCompletingDispatcher {
        calls: AtomicUsize::new(0),
    });
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider,
        store.clone(),
        Some(dispatcher.clone()),
    );
    let task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("fail the reset commit"))
        .await
        .expect("submit")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert!(
        outcome
            .error
            .expect("store error")
            .message
            .contains("repair reset failure")
    );
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 0);
    assert_items_closed_before_terminal(&store.inner.events(&SessionId::new(SESSION)).await);
    handle.stop().await.expect("stop");
    task.await.expect("join");
}

#[tokio::test]
async fn repair_recovery_read_failure_leaves_the_checkpoint_recoverable() {
    let (_, events, _, _) = toolrepair_run(
        config(),
        vec![
            FakeStep::EmitToolCall {
                call_id: "open-on-restart".into(),
                name: "inspect".into(),
                args: serde_json::json!({}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
    )
    .await;
    let run_id = events[0].run_id.clone().expect("run id");
    let (index, item_id) = events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match typed(event) {
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::ToolCall { .. },
            }) => Some((index, item_id)),
            _ => None,
        })
        .expect("open item");
    let mut prefix = events[..=index].to_vec();
    let store = Arc::new(BatchRecordingStore {
        reject_repair_read: true,
        ..BatchRecordingStore::new()
    });
    StoreHandle::append(&store.inner, &mut prefix)
        .await
        .expect("restore durable open item");
    let provider = Arc::new(FakeProvider::new(vec![]));
    let handle = HarnessActor::spawn(config(), provider.clone(), store.clone());
    let outcome = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id,
            messages: vec![Message::user_text("recover")],
            checkpoint: RouteWaitCheckpoint {
                tools: vec![RouteWaitToolCheckpoint {
                    item_id,
                    call_id: "open-on-restart".into(),
                    name: "inspect".into(),
                    args: "{}".into(),
                }],
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("resume")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert!(
        outcome
            .error
            .expect("store read error")
            .message
            .contains("recovery read failure")
    );
    assert!(provider.requests().is_empty());
    assert_eq!(
        store.inner.events(&SessionId::new(SESSION)).await,
        prefix,
        "read failure must not seal the still-open checkpoint"
    );
    handle.stop().await.expect("stop");
}

/// MUTATION CHECK: replace the four-envelope settlement append with sequential
/// commits. Expected failure: the injected batch rejection is bypassed and the
/// turn succeeds, or one of the rejected facts leaks into the journal.
#[tokio::test]
async fn rejected_tool_settlement_batch_publishes_none_of_its_facts() {
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::EmitToolCall {
        call_id: "reject-atomic-tool".into(),
        name: "inspect".into(),
        args: serde_json::json!({"path":"src/lib.rs"}),
    }]));
    let store = Arc::new(BatchRecordingStore::rejecting_tool_settlement());
    let (actor, handle) = HarnessActor::new_with_dispatcher(
        config(),
        provider,
        store.clone(),
        Some(Arc::new(CompletingDispatcher)),
    );
    let actor_task = tokio::spawn(actor.run());

    let outcome = handle
        .submit_turn(SubmitTurn::new("reject atomic settlement"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert!(store.rejected_tool_settlement());

    let events = store.inner.events(&SessionId::new(SESSION)).await;
    assert!(!events.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::ToolResult { ref call_id, .. } if call_id == "reject-atomic-tool"
        )
    }));
    assert!(!events.iter().any(|event| {
        matches!(
            typed(event),
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::ToolCall {
                    ref call_id,
                    status: ToolStatus::Completed,
                    ..
                },
                ..
            }) if call_id == "reject-atomic-tool"
        )
    }));
    let running_tool = events
        .iter()
        .rposition(|event| matches!(typed(event), EventPayload::RunState(RunState::RunningTool)))
        .expect("RunningTool committed before dispatch");
    assert!(
        !events[running_tool + 1..]
            .iter()
            .any(|event| { matches!(typed(event), EventPayload::RunState(RunState::Streaming)) })
    );

    handle.stop().await.expect("actor stops");
    actor_task.await.expect("actor joins");
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

    async fn branch_lineage(
        &self,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
    ) -> Result<Vec<BranchDescriptor>, HaiderError> {
        self.inner.branch_lineage(session_id, branch_id).await
    }
}

struct HangingStartProvider {
    entered: Notify,
}

struct ImmediateErrorProvider {
    error: ProviderError,
}

struct NetworkFailureThenNeverOpensProvider {
    route: Arc<Mutex<RouteStatus>>,
    requests: AtomicUsize,
}

impl NetworkFailureThenNeverOpensProvider {
    fn new(route: Arc<Mutex<RouteStatus>>) -> Self {
        Self {
            route,
            requests: AtomicUsize::new(0),
        }
    }
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
    cache_account: Mutex<Option<(String, Option<u64>)>>,
}

struct ScriptedRotationResolver {
    calls: AtomicUsize,
    first: Arc<dyn Provider>,
    first_alias: CredentialAlias,
    second: Arc<dyn Provider>,
    second_alias: CredentialAlias,
}

struct CapabilityFallbackResolver {
    calls: AtomicUsize,
    provider: Arc<dyn Provider>,
}

impl std::fmt::Debug for CapabilityFallbackResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapabilityFallbackResolver")
            .field("calls", &self.calls.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderAttemptResolver for CapabilityFallbackResolver {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        assert_eq!(
            error.presentation.subcode.as_str(),
            "provider-web-tool-rejected"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ProviderAttemptDecision::Fallback {
            provider: Arc::clone(&self.provider),
            account: current_account.clone(),
        })
    }
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

/// H2: a resolver that keeps deciding `Retry` (credential refresh) — the exact
/// shape a persistently-failing 401 produces. The `escape_after` hatch returns
/// `Stop` once the call count crosses it so an UNBUDGETED (regressed) build
/// still terminates the test rather than spinning forever.
struct RefreshingAttemptResolver {
    calls: AtomicUsize,
    provider: Arc<dyn Provider>,
    escape_after: usize,
}

impl std::fmt::Debug for RefreshingAttemptResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RefreshingAttemptResolver")
            .field("calls", &self.calls.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ProviderAttemptResolver for RefreshingAttemptResolver {
    async fn resolve(
        &self,
        current_account: &CredentialAlias,
        _error: &ProviderError,
    ) -> Result<ProviderAttemptDecision, HaiderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call >= self.escape_after {
            return Ok(ProviderAttemptDecision::Stop);
        }
        // Same provider + same account = a pure credential refresh (no rotation).
        Ok(ProviderAttemptDecision::Retry {
            provider: Arc::clone(&self.provider),
            account: current_account.clone(),
        })
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
impl Provider for NetworkFailureThenNeverOpensProvider {
    fn trusts_default_route_absence(&self) -> bool {
        true
    }

    fn route_status(&self) -> RouteStatus {
        *self.route.lock().expect("route lock")
    }

    async fn capabilities(&self) -> CapabilityDoc {
        FakeProvider::new(Vec::new()).capabilities().await
    }

    async fn stream_turn(&self, _request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::NetworkUnavailable,
                "connection failed before response headers",
            ));
        }
        pending().await
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

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        *self.cache_account.lock().expect("cache account lock") =
            request.cache_metadata.and_then(|metadata| {
                metadata
                    .account_scope
                    .map(|account| (account, metadata.reuse_gap_ms))
            });
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

/// LAW (LW2, pause_turn resend + W-B display channels): a `pause_turn`
/// finish resends the PAUSED ASSISTANT MESSAGE UNCHANGED — captured opaque
/// server-tool facts included, and with NO synthesized user nudge — in the
/// same logical run under the shared continuation checkpoint. The display
/// channels journal exactly once and prompt-OMITTED: the server tool row
/// commits as a closed ToolCall pair, its bounded result rides a ToolResult
/// fact, and the deduped, bounded sources list lands under the finished
/// message. Replay authority stays with the opaque channel alone.
#[tokio::test]
async fn pause_turn_resends_the_paused_assistant_unchanged_and_journals_web_activity() {
    let server_use = serde_json::json!({
        "type": "server_tool_use",
        "id": "srvtoolu_1",
        "name": "web_search",
        "input": {"query": "rust sse"},
    });
    let server_result = serde_json::json!({
        "type": "web_search_tool_result",
        "tool_use_id": "srvtoolu_1",
        "content": [{"type": "web_search_result", "url": "https://example.com/a", "title": "A", "encrypted_content": "ENC_A"}],
    });
    let sources = vec![
        haider_protocol::provider::WebSource {
            url: "https://example.com/a".into(),
            title: Some("A".into()),
        },
        haider_protocol::provider::WebSource {
            url: "https://example.com/b".into(),
            title: None,
        },
        // Duplicate URL — the journaled list must dedup it.
        haider_protocol::provider::WebSource {
            url: "https://example.com/a".into(),
            title: Some("A again".into()),
        },
    ];
    let (handle, store, provider) = runtime(vec![
        FakeStep::EmitText {
            text: "checking".into(),
        },
        FakeStep::EmitProviderOpaque {
            provider: "anthropic".into(),
            data: server_use.clone(),
        },
        FakeStep::EmitServerToolUse {
            call_id: "srvtoolu_1".into(),
            name: "web_search".into(),
            args: serde_json::json!({"query": "rust sse"}),
        },
        FakeStep::Finish {
            reason: FinishReason::PauseTurn,
        },
        FakeStep::EmitProviderOpaque {
            provider: "anthropic".into(),
            data: server_result.clone(),
        },
        FakeStep::EmitServerToolResult {
            call_id: "srvtoolu_1".into(),
            preview: "1 result".into(),
            is_error: false,
        },
        FakeStep::EmitWebSources { sources },
        FakeStep::EmitText {
            text: "answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]);

    let outcome = handle
        .submit_turn(SubmitTurn::new("search the web"))
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(outcome.finish_reason, FinishReason::EndTurn);

    // The paused assistant message is resent UNCHANGED, with NO user nudge.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "pause_turn continues in the same run");
    let resumed = requests[1]
        .messages
        .last()
        .expect("resumed request has messages");
    assert_eq!(
        resumed.role,
        haider_provider::MessageRole::Assistant,
        "the request ends with the paused assistant message — no synthesized user turn"
    );
    assert_eq!(
        resumed.blocks,
        vec![
            Block::Text {
                text: "checking".into(),
            },
            Block::ProviderOpaque {
                provider: "anthropic".into(),
                data: server_use.into(),
            },
        ],
        "the paused assistant message replays verbatim, opaque facts included"
    );
    let all_text = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.to_owned_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !all_text
            .iter()
            .any(|text| text
                == "Continue exactly where you stopped. Do not repeat completed content."),
        "pause_turn must NOT inject the MaxTokens continuation nudge"
    );

    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);

    // Shared continuation checkpoint, honest reason.
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
        .expect("continuation checkpoint journaled");
    assert_eq!(checkpoint.reason, FinishReason::PauseTurn);

    // The server tool row: one closed ToolCall pair, prompt-OMITTED.
    let row_envelopes = events
        .iter()
        .filter(|event| {
            matches!(
                typed(event),
                EventPayload::Item(ItemEvent::Started {
                    item: TurnItem::ToolCall { .. },
                    ..
                }) | EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall { .. },
                    ..
                })
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(row_envelopes.len(), 2, "exactly one closed server tool row");
    for envelope in &row_envelopes {
        assert_eq!(
            envelope.render.prompt,
            PromptRender::Omit,
            "server rows must never re-enter prompts — replay rides the opaque channel"
        );
        assert!(envelope.render.ui, "server rows are UI-visible");
    }
    let completed_row = row_envelopes
        .iter()
        .find_map(|event| match typed(event) {
            EventPayload::Item(ItemEvent::Completed {
                item:
                    TurnItem::ToolCall {
                        call_id,
                        name,
                        args,
                        status,
                    },
                ..
            }) => Some((call_id, name, args, status)),
            _ => None,
        })
        .expect("completed server row");
    assert_eq!(
        completed_row,
        (
            "srvtoolu_1".into(),
            "web_search".into(),
            serde_json::json!({"query": "rust sse"}),
            ToolStatus::Completed,
        )
    );

    // The bounded result fact, prompt-OMITTED.
    let result_fact = events
        .iter()
        .find(|event| {
            matches!(
                typed(event),
                EventPayload::ToolResult { ref call_id, .. } if call_id == "srvtoolu_1"
            )
        })
        .expect("server tool result fact");
    assert_eq!(result_fact.render.prompt, PromptRender::Omit);

    // The deduped, UI-visible sources list — journaled exactly once.
    let sources_envelopes = events
        .iter()
        .filter(|event| {
            matches!(
                typed(event),
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Extension { ref kind, .. },
                    ..
                }) if kind == haider_protocol::provider::WEB_SOURCES_EXTENSION_KIND
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(sources_envelopes.len(), 1, "sources journal exactly once");
    assert_eq!(sources_envelopes[0].render.prompt, PromptRender::Omit);
    assert!(sources_envelopes[0].render.ui);
    let data = match typed(sources_envelopes[0]) {
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::Extension { data, .. },
            ..
        }) => data,
        _ => unreachable!("filtered above"),
    };
    assert_eq!(
        data,
        serde_json::json!({
            "sources": [
                {"url": "https://example.com/a", "title": "A"},
                {"url": "https://example.com/b"},
            ]
        }),
        "sources dedup by URL and keep arrival order"
    );
}

#[path = "support/request_budget_laws.rs"]
mod request_budget_laws;

struct AppliedFailureDispatcher {
    calls: AtomicUsize,
    effects: Vec<haider_protocol::tool::ToolFileEffect>,
}

#[async_trait]
impl ToolDispatcher for AppliedFailureDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &haider_core::CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut error = HaiderError::new(
            ErrorCode::Internal,
            "change ledger failed: injected post-apply evidence failure",
            false,
        );
        error.details = Some(serde_json::json!({"applied_effects": self.effects}));
        Err(error)
    }
}

/// A fatal post-apply failure remains fatal, while the already-applied file
/// effects are journaled once before the failed tool closes and the run ends.
#[tokio::test]
async fn toolshape_fatal_applied_effects_are_journaled_once_before_errored_and_replay() {
    use haider_protocol::tool::{ToolFileEffect, ToolFileEffectKind, ToolResultStatus};
    let effects = vec![
        ToolFileEffect {
            kind: ToolFileEffectKind::Delete,
            name: "before.txt".into(),
            path: "before.txt".into(),
            absolute_path: "/workspace/before.txt".into(),
            bytes: 14,
        },
        ToolFileEffect {
            kind: ToolFileEffectKind::Create,
            name: "after.txt".into(),
            path: "after.txt".into(),
            absolute_path: "/workspace/after.txt".into(),
            bytes: 14,
        },
    ];
    let dispatcher = Arc::new(AppliedFailureDispatcher {
        calls: AtomicUsize::new(0),
        effects: effects.clone(),
    });
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitToolCall {
            call_id: "applied-fixture".into(),
            name: "fs_path".into(),
            args: serde_json::json!({"operation":"move","source":"before.txt","destination":"after.txt"}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
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
        Some(dispatcher.clone()),
    );
    let actor_task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_turn(SubmitTurn::new("move fixture"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("turn outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let error = outcome.error.expect("original fatal error");
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(
        error.message,
        "change ledger failed: injected post-apply evidence failure"
    );
    assert!(!error.retryable);
    assert_eq!(
        provider.requests().len(),
        1,
        "fatal failure cannot request another provider response"
    );
    assert_eq!(dispatcher.calls.load(Ordering::SeqCst), 1);
    let events = store.events(&SessionId::new(SESSION)).await;
    assert_items_closed_before_terminal(&events);
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match typed(event) {
            EventPayload::ToolResult { call_id, result } => Some((event, call_id, result)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    let (envelope, call_id, result) = &results[0];
    assert_eq!(call_id, "applied-fixture");
    assert_eq!(result.status, ToolResultStatus::Failed);
    assert_eq!(result.effects, effects);
    assert!(result.truncation.is_none());
    let payload = envelope.payload.to_json_value();
    assert_eq!(
        payload
            .pointer("/effects/0/path")
            .and_then(serde_json::Value::as_str),
        Some("before.txt")
    );
    assert_eq!(
        payload
            .pointer("/effects/1/name")
            .and_then(serde_json::Value::as_str),
        Some("after.txt")
    );
    let failed_item = events.iter().find(|event| matches!(typed(event), EventPayload::Item(ItemEvent::Completed { item: TurnItem::ToolCall { ref call_id, status: ToolStatus::Failed, .. }, .. }) if call_id == "applied-fixture")).expect("failed item closes");
    assert!(envelope.seq < failed_item.seq);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(typed(event), EventPayload::RunState(RunState::Errored)))
            .count(),
        1
    );
    let mut replay = Vec::new();
    let mut cursor = 0;
    loop {
        let page = store
            .read(&SessionId::new(SESSION), cursor, 7)
            .await
            .expect("replay page");
        if page.is_empty() {
            break;
        }
        cursor = page.last().expect("nonempty page").seq;
        replay.extend(page);
    }
    assert_eq!(
        serde_json::to_vec(&replay).expect("replay JSON"),
        serde_json::to_vec(&events).expect("live journal JSON")
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "replay has no provider effects"
    );
    handle.stop().await.expect("stop actor");
    actor_task.await.expect("join actor");
}
