#![allow(clippy::expect_used)]

use async_trait::async_trait;
use haider_core::{
    CancelToken, HarnessActor, HarnessConfig, MemoryStore, RetrySleeper, SubmitTurn,
    ToolDispatchResult, ToolDispatcher, retry_jittered_backoff_ms,
};
use haider_protocol::EventPayload;
use haider_protocol::error::{ErrorAction, ErrorScope, HaiderError};
use haider_protocol::ids::{CredentialAlias, DeviceId, ItemId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, ToolStatus, TurnItem};
use haider_protocol::menu::{AnswerVia, ErrorRecoveryCardKind, MenuAnswer, MenuKind};
use haider_protocol::provider::{Block, FinishReason};
use haider_protocol::state::RunState;
use haider_protocol::tool::{BoundedResult, ToolResultStatus};
use haider_provider::{FakeProvider, FakeStep, MessageRole, ProviderErrorKind};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct RecordingSleeper(Mutex<Vec<u64>>);

#[derive(Debug)]
struct StatusDispatcher {
    status: ToolResultStatus,
    reason: Option<String>,
}

#[async_trait]
impl ToolDispatcher for StatusDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        Ok(ToolDispatchResult::Completed(BoundedResult {
            preview: "tool result".into(),
            truncated: false,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: self.status,
            reason: self.reason.clone(),
            presentation: None,
        }))
    }
}

#[async_trait]
impl RetrySleeper for RecordingSleeper {
    async fn sleep(&self, delay_ms: u64) {
        self.0.lock().expect("sleeper lock").push(delay_ms);
    }
}

fn spawn(
    session: &str,
    script: Vec<FakeStep>,
    sleeper: Arc<RecordingSleeper>,
) -> (
    haider_core::HarnessHandle,
    Arc<MemoryStore>,
    Arc<FakeProvider>,
) {
    let mut config = HarnessConfig::for_session(SessionId::new(session), DeviceId::new("e1"), 1, 1);
    config.retry_sleeper = sleeper;
    let provider = Arc::new(FakeProvider::new(script));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    (handle, store, provider)
}

/// LAW E1a: the actor copies the dispatcher terminal status instead of
/// unconditionally closing the tool as Completed. MUTATION: hard-code
/// `ToolStatus::Completed` at that join and the rejected assertion fails.
#[tokio::test]
async fn e1a_actor_preserves_failed_and_successful_tool_status() {
    for (suffix, result_status, expected) in [
        ("denied", ToolResultStatus::Rejected, ToolStatus::Rejected),
        ("ok", ToolResultStatus::Completed, ToolStatus::Completed),
    ] {
        let session = SessionId::new(format!("e1-tool-{suffix}"));
        let provider = Arc::new(FakeProvider::new(vec![
            FakeStep::EmitToolCall {
                call_id: format!("call-{suffix}"),
                name: "fs_write".into(),
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
        let config = HarnessConfig::for_session(session.clone(), DeviceId::new("e1"), 1, 1);
        let (actor, handle) = HarnessActor::new_with_dispatcher(
            config,
            provider,
            store.clone(),
            Some(Arc::new(StatusDispatcher {
                status: result_status,
                reason: (result_status != ToolResultStatus::Completed)
                    .then(|| "effect denied by policy".into()),
            })),
        );
        let actor_task = tokio::spawn(actor.run());
        let outcome = handle
            .submit_turn(SubmitTurn::new("tool"))
            .await
            .expect("accepted")
            .wait()
            .await
            .expect("outcome");
        assert_eq!(outcome.state, RunState::Done);
        let payloads = store
            .events(&session)
            .await
            .into_iter()
            .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
            .collect::<Vec<_>>();
        let completed = payloads
            .iter()
            .find_map(|payload| match payload {
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::ToolCall { status, .. },
                    ..
                }) => Some(*status),
                _ => None,
            })
            .expect("completed tool row");
        assert_eq!(completed, expected);
        let result = payloads
            .iter()
            .find_map(|payload| match payload {
                EventPayload::ToolResult { result, .. } => Some(result),
                _ => None,
            })
            .expect("tool result row");
        if result_status == ToolResultStatus::Completed {
            assert!(result.presentation.is_none());
        } else {
            let presentation = result
                .presentation
                .as_ref()
                .expect("failed tool result has typed presentation");
            assert_eq!(presentation.scope, ErrorScope::Tool);
            assert!(!presentation.subcode.as_str().is_empty());
            assert!(!presentation.allowed_actions.is_empty());
        }
        drop(handle);
        actor_task.await.expect("actor task");
    }
}

/// LAW E1b: refusal-only responses leave a durable, visible refusal row and
/// finish Done. MUTATION: deleting the RefusalDelta/surfacing arm removes the
/// row and fails this test at runtime.
#[tokio::test]
async fn e1b_refusal_only_is_visible_done_and_nonempty() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, _) = spawn(
        "e1-refusal",
        vec![
            FakeStep::EmitRefusal {
                text: "  I cannot help with that.\n".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::Refusal,
            },
        ],
        sleeper,
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("refuse"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    let events = store.events(&SessionId::new("e1-refusal")).await;
    let refusals = events
        .iter()
        .filter_map(|envelope| {
            serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()
        })
        .filter_map(|payload| match payload {
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::Refusal { reason },
                ..
            }) => Some(reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(refusals, vec!["I cannot help with that."]);
}

#[tokio::test]
async fn e1b_normal_completion_has_no_refusal_row() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, _) = spawn(
        "e1-normal",
        vec![
            FakeStep::EmitText { text: "ok".into() },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper,
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("answer"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert!(
        store
            .events(&SessionId::new("e1-normal"))
            .await
            .iter()
            .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload.clone()).ok())
            .all(|payload| !matches!(
                payload,
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::Refusal { .. },
                    ..
                })
            ))
    );
}

/// LAW E1e: a clean pre-content EOF is a retryable interruption and recovery
/// on the next attempt completes. MUTATION: reclassifying EOF as malformed
/// makes this terminal and fails the attempt/wait assertions.
#[tokio::test]
async fn e1e_premature_eof_before_content_retries_and_recovers() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        "e1-eof",
        vec![
            FakeStep::PrematureEof,
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("recover"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2);
    let run_id = store
        .events(&SessionId::new("e1-eof"))
        .await
        .into_iter()
        .find_map(|event| event.run_id)
        .expect("run id");
    assert_eq!(
        *sleeper.0.lock().expect("sleeper lock"),
        vec![retry_jittered_backoff_ms(&run_id, 1)]
    );
}

/// LAW E1c: quota never spends retry budget even if a broken classifier marks
/// it retryable. MUTATION: removing the kind gate yields another attempt.
#[tokio::test]
async fn e1c_quota_exhaustion_terminalizes_without_retry_budget() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, _, provider) = spawn(
        "e1-quota",
        vec![FakeStep::ErrorWithRetryability {
            kind: ProviderErrorKind::QuotaExhausted,
            message: "provider quota/credit exhausted — retrying will not help".into(),
            retryable: true,
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("quota"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    assert!(sleeper.0.lock().expect("sleeper lock").is_empty());
}

/// LAW E3a: billing exhaustion is terminal, exposes TopUp recovery, and never
/// spends retry budget. MUTATION: admitting QuotaExhausted to the retry-kind
/// gate makes the request-count assertion fail before the card is observed.
#[tokio::test]
async fn e3a_quota_exhaustion_card_has_top_up_and_zero_retries() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        "e3-quota-card",
        vec![FakeStep::ErrorWithRetryability {
            kind: ProviderErrorKind::QuotaExhausted,
            message: "RAW_QUOTA_BODY_MARKER".into(),
            retryable: true,
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("quota"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    assert!(sleeper.0.lock().expect("sleeper lock").is_empty());
    let payloads = store
        .events(&SessionId::new("e3-quota-card"))
        .await
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .collect::<Vec<_>>();
    let presentation = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::MenuOpened(menu) => match &menu.kind {
                MenuKind::ErrorRecovery {
                    card: ErrorRecoveryCardKind::QuotaExhausted,
                    presentation,
                    ..
                } => Some(presentation),
                _ => None,
            },
            _ => None,
        })
        .expect("quota recovery card");
    assert_eq!(presentation.subcode.as_str(), "quota-exhausted");
    assert!(presentation.allowed_actions.contains(&ErrorAction::TopUp));
    assert!(!presentation.allowed_actions.contains(&ErrorAction::Retry));
    assert!(
        !serde_json::to_string(presentation)
            .expect("presentation JSON")
            .contains("RAW_QUOTA_BODY_MARKER")
    );
}

/// LAW E3b producer half: OAuth-scoped authentication rejection becomes the
/// typed re-login/re-adopt card rather than a generic provider failure.
#[tokio::test]
async fn e3b_oauth_expired_produces_relogin_card() {
    let session = SessionId::new("e3-oauth-card");
    let provider = Arc::new(FakeProvider::new(vec![FakeStep::ErrorWithRetryability {
        kind: ProviderErrorKind::Authentication,
        message: "RAW_OAUTH_BODY_MARKER".into(),
        retryable: false,
        retry_after_ms: None,
    }]));
    let store = Arc::new(MemoryStore::new());
    let mut config = HarnessConfig::for_session(session.clone(), DeviceId::new("e3"), 1, 1);
    config.usage_scope.provider = "openai-oauth".into();
    config.usage_scope.auth_scope = "oauth_subscription".into();
    config.usage_account = Some(CredentialAlias::new("openai-oauth"));
    let handle = HarnessActor::spawn(config, provider, store.clone());
    let outcome = handle
        .submit_turn(SubmitTurn::new("authenticate"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    let payloads = store
        .events(&session)
        .await
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .collect::<Vec<_>>();
    let presentation = payloads
        .iter()
        .find_map(|payload| match payload {
            EventPayload::MenuOpened(menu) => match &menu.kind {
                MenuKind::ErrorRecovery {
                    card: ErrorRecoveryCardKind::OauthExpired,
                    presentation,
                    provider: Some(provider),
                    account: Some(account),
                    ..
                } if provider == "openai-oauth" && account.as_str() == "openai-oauth" => {
                    Some(presentation)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("OAuth recovery card");
    assert_eq!(presentation.subcode.as_str(), "oauth-expired");
    assert!(presentation.allowed_actions.contains(&ErrorAction::Relogin));
    assert!(
        presentation
            .allowed_actions
            .contains(&ErrorAction::Reimport)
    );
    assert!(
        !serde_json::to_string(presentation)
            .expect("presentation JSON")
            .contains("RAW_OAUTH_BODY_MARKER")
    );
}

/// LAW E4a: a post-content EOF closes the exact item as incomplete and parks
/// on a choice card instead of silently committing Done. MUTATION: completing
/// the accumulator as AgentMessage makes the marker assertion fail.
#[tokio::test]
async fn e4a_midstream_failure_journals_incomplete_item_and_choice_card() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, store, provider) = spawn(
        "e4-marker",
        vec![
            FakeStep::EmitText {
                text: "partial response".into(),
            },
            FakeStep::PrematureEof,
            FakeStep::EmitText {
                text: "fresh response".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper,
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("answer"))
        .await
        .expect("accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor stays available")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("input state") else {
        panic!("expected input state");
    };
    let payloads = store
        .events(&SessionId::new("e4-marker"))
        .await
        .into_iter()
        .filter_map(|event| serde_json::from_value::<EventPayload>(event.payload).ok())
        .collect::<Vec<_>>();
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::IncompleteAgentMessage { text, .. },
            ..
        }) if text == "partial response"
    )));
    assert!(payloads.iter().any(|payload| matches!(
        payload,
        EventPayload::MenuOpened(haider_protocol::menu::Menu {
            kind: MenuKind::ErrorRecovery {
                card: ErrorRecoveryCardKind::PartialStream,
                ..
            },
            blocking: true,
            ..
        })
    )));
    assert!(
        !payloads
            .iter()
            .any(|payload| matches!(payload, EventPayload::RunState(RunState::Done)))
    );
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: Some("retry_fresh".into()),
            option_index: 1,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("retry fresh answer");
    assert_eq!(
        turn.wait().await.expect("turn outcome").state,
        RunState::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    // LAW extension (orchestrator): retry-fresh OMITS the partial from the
    // fresh provider prompt — the transcript keeps the incomplete item, the
    // model never sees it. Pinned after the leak-the-partial mutation
    // SURVIVED this suite unpinned.
    assert!(
        !requests[1].messages.iter().any(|message| {
            message.blocks.iter().any(
                |block| matches!(block, Block::Text { text } if text.contains("partial response")),
            )
        }),
        "retry-fresh prompt must not carry the interrupted partial"
    );
}

/// LAW E4b: ContinuePartial primes the new request with the exact partial and
/// continuation instruction once. MUTATION: pushing either message twice is
/// caught by the exact counts below.
#[tokio::test]
async fn e4b_continue_partial_primes_followup_exactly_once() {
    const PARTIAL: &str = "alpha beta";
    const INSTRUCTION: &str = "The previous response was interrupted. Continue exactly where it stopped without repeating any completed text.";
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, _, provider) = spawn(
        "e4-continue",
        vec![
            FakeStep::EmitText {
                text: PARTIAL.into(),
            },
            FakeStep::PrematureEof,
            FakeStep::EmitText {
                text: " gamma".into(),
            },
            FakeStep::Finish {
                reason: FinishReason::EndTurn,
            },
        ],
        sleeper,
    );
    let turn = handle
        .submit_turn(SubmitTurn::new("continue"))
        .await
        .expect("accepted");
    let mut state = handle.state_receiver();
    let parked = state
        .wait_for(|state| matches!(state, Some(RunState::InputRequired { .. })))
        .await
        .expect("actor stays available")
        .clone();
    let RunState::InputRequired { menu } = parked.expect("input state") else {
        panic!("expected input state");
    };
    handle
        .answer_menu(MenuAnswer {
            menu,
            option_key: Some("continue_partial".into()),
            option_index: 0,
            value: None,
            via: AnswerVia::Rpc,
        })
        .await
        .expect("continue answer");
    assert_eq!(
        turn.wait().await.expect("turn outcome").state,
        RunState::Done
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    let count = |role: MessageRole, needle: &str| {
        second
            .iter()
            .filter(|message| message.role == role)
            .flat_map(|message| &message.blocks)
            .filter(|block| matches!(block, Block::Text { text } if text == needle))
            .count()
    };
    assert_eq!(count(MessageRole::Assistant, PARTIAL), 1);
    assert_eq!(count(MessageRole::User, INSTRUCTION), 1);
}

#[tokio::test]
async fn e1e_permanent_connection_configuration_does_not_retry() {
    let sleeper = Arc::new(RecordingSleeper::default());
    let (handle, _, provider) = spawn(
        "e1-permanent-connection",
        vec![FakeStep::ErrorWithRetryability {
            kind: ProviderErrorKind::ConnectionConfiguration,
            message: "certificate trust failure; check endpoint/proxy configuration".into(),
            retryable: true,
            retry_after_ms: None,
        }],
        sleeper.clone(),
    );
    let outcome = handle
        .submit_turn(SubmitTurn::new("connect"))
        .await
        .expect("accepted")
        .wait()
        .await
        .expect("outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(provider.requests().len(), 1);
    assert!(sleeper.0.lock().expect("sleeper lock").is_empty());
}
