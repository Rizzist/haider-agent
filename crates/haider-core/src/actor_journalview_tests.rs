#![allow(clippy::expect_used)]

use super::*;
use crate::MemoryStore;
use haider_protocol::agent::{AgentRole, Grant, Placement};
use haider_protocol::ids::LeaseId;
use haider_provider::{FakeProvider, FakeStep};
use std::collections::BTreeMap;

#[derive(Debug)]
struct RefusingNarrativeRequestGuard {
    admitted: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ProviderBudgetGuard for RefusingNarrativeRequestGuard {
    async fn before_request(
        &self,
        _run_id: &RunId,
        _provider: &str,
        _request: &TurnRequest,
        _projected_input_tokens: u64,
    ) -> Result<ProviderBudgetPermit, ProviderBudgetGuardError> {
        self.admitted
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .map(|_| ProviderBudgetPermit::new(()))
            .map_err(|_| ProviderBudgetGuardError::Cancelled)
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
async fn journalview_admission_refusal_never_invents_a_physical_request() {
    for admitted in [0, 1] {
        let session = SessionId::new(format!("journalview-refusal-{admitted}"));
        let run_id = RunId::new(format!("journalview-refusal-run-{admitted}"));
        let mut config =
            HarnessConfig::for_session(session.clone(), DeviceId::new("journalview-device"), 1, 1);
        config.provider_budget_guard = Some(Arc::new(RefusingNarrativeRequestGuard {
            admitted: std::sync::atomic::AtomicUsize::new(admitted),
        }));
        let provider = Arc::new(FakeProvider::new(vec![
            FakeStep::EmitText {
                text: "before the tool".into(),
            },
            FakeStep::EmitToolCall {
                call_id: "refusal-tool".into(),
                name: "todo_write".into(),
                args: serde_json::json!({"items": []}),
            },
            FakeStep::Finish {
                reason: FinishReason::ToolUse,
            },
        ]));
        let store = Arc::new(MemoryStore::new());
        let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
        let outcome = handle
            .submit_committed_turn(SubmitCommittedTurn {
                run_id,
                messages: vec![Message::user_text("refuse before transport")],
            })
            .await
            .expect("accept")
            .wait()
            .await
            .expect("outcome");
        assert_eq!(outcome.state, RunState::Cancelled);
        assert_eq!(provider.requests().len(), admitted);
        let journal = store.events(&session).await;
        let requests = journal
            .iter()
            .filter_map(|event| event.payload.get("provider_request"))
            .map(|request| request["request_ordinal"].as_u64().expect("ordinal"))
            .collect::<HashSet<_>>();
        assert_eq!(
            requests,
            (1..=admitted as u64).collect::<HashSet<_>>(),
            "refused reserved ordinal never reaches raw journal or normalized rounds"
        );
        if admitted == 1 {
            let terminal = journal.last().expect("terminal");
            assert_eq!(terminal.payload["provider_request"]["request_ordinal"], 1);
            assert_eq!(
                terminal.payload["provider_finish_reason"], "tool_use",
                "refusal keeps the actual preceding request's Finish"
            );
            assert!(journal.iter().any(|event| matches!(
                event.payload.decode_event(),
                Ok(EventPayload::ToolResult { .. })
            )));
        }
        handle.stop().await.expect("stop");
    }
}

#[derive(Debug, Default)]
struct FailingNarrativeRebindResolver {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl ProviderRebindResolver for FailingNarrativeRebindResolver {
    async fn refresh(
        &self,
        _model: &str,
        _reasoning: &str,
    ) -> Result<Option<ProviderRebindTarget>, HaiderError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(HaiderError::new(
            ErrorCode::Internal,
            "journalview fixture rebind refresh failed",
            false,
        ))
    }
}

/// A recovered route checkpoint owns live item lifecycles before the next
/// provider route is resolved. Resolver failure must settle those exact items
/// under their original request before publishing the failure terminal.
#[tokio::test]
async fn journalview_rebind_failure_closes_recovered_items_under_the_source_request() {
    let session_id = SessionId::new("journalview-rebind-session");
    let run_id = RunId::new("journalview-rebind-run");
    let message_id = ItemId::new("journalview-rebind-text");
    let reasoning_id = ItemId::new("journalview-rebind-reasoning");
    let tool_id = ItemId::new("journalview-rebind-tool");
    let resolver = Arc::new(FailingNarrativeRebindResolver::default());
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("journalview-rebind-device"),
        7,
        12,
    );
    config.turn_ordinal = 9;
    config.provider_request_ordinal_already_made = 2;
    config.provider_rebind_resolver = Some(resolver.clone());
    let provider = Arc::new(FakeProvider::new(Vec::new()));
    let store = Arc::new(MemoryStore::new());
    let (mut actor, handle) =
        HarnessActor::new_with_dispatcher(config, provider.clone(), store.clone(), None);
    let source_request = ProviderRequestAttemptV1 {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        turn_ordinal: 9,
        request_ordinal: 2,
        request_kind: ProviderRequestKind::Primary,
    };
    let source_json = serde_json::to_value(&source_request).expect("source request");
    actor.narrative_request = Some(source_request);
    for (item_id, item, delta) in [
        (
            message_id.clone(),
            TurnItem::AgentMessage {
                text: ReplyText::default(),
            },
            ItemDelta::Text {
                text: "emitted before route loss".into(),
            },
        ),
        (
            reasoning_id.clone(),
            TurnItem::Reasoning {
                summary: ReplyText::default(),
            },
            ItemDelta::Reasoning {
                text: "reasoning before route loss".into(),
            },
        ),
        (
            tool_id.clone(),
            TurnItem::ToolCall {
                call_id: "journalview-rebind-call".into(),
                name: "todo_write".into(),
                args: serde_json::json!({}),
                status: ToolStatus::InProgress,
            },
            ItemDelta::ToolArgs {
                fragment: "{\"items\":".into(),
            },
        ),
    ] {
        actor
            .commit_item(
                &run_id,
                ItemEvent::Started {
                    item_id: item_id.clone(),
                    item,
                },
            )
            .await
            .expect("seed open item");
        actor
            .commit_item(&run_id, ItemEvent::Delta { item_id, delta })
            .await
            .expect("seed emitted content");
    }
    actor
        .commit_state(
            &run_id,
            RunState::Waiting {
                reason: WaitReason::NetworkUnavailable,
            },
        )
        .await
        .expect("seed route wait");
    // Force recovery to read durable ownership, as a fresh actor would.
    actor.narrative_request = None;
    let seed_head = store.latest_seq(&session_id).await.expect("seed head");
    let mut live = handle.subscribe();
    let actor_task = tokio::spawn(actor.run());
    let outcome = handle
        .submit_route_wait_turn(SubmitRouteWaitTurn {
            run_id,
            messages: vec![Message::user_text("recover the interrupted response")],
            checkpoint: RouteWaitCheckpoint {
                message: Some(RouteWaitTextCheckpoint {
                    item_id: message_id.clone(),
                    text: "emitted before route loss".into(),
                }),
                reasoning: Some(RouteWaitTextCheckpoint {
                    item_id: reasoning_id.clone(),
                    text: "reasoning before route loss".into(),
                }),
                tools: vec![RouteWaitToolCheckpoint {
                    item_id: tool_id.clone(),
                    call_id: "journalview-rebind-call".into(),
                    name: "todo_write".into(),
                    args: "{\"items\":".into(),
                }],
                ..RouteWaitCheckpoint::default()
            },
        })
        .await
        .expect("accept recovered route checkpoint")
        .wait()
        .await
        .expect("rebind failure outcome");
    assert_eq!(outcome.state, RunState::Errored);
    assert_eq!(
        outcome.error.expect("refresh error").message,
        "journalview fixture rebind refresh failed"
    );
    assert_eq!(resolver.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        provider.requests().is_empty(),
        "no provider request was sent"
    );
    let suffix = store
        .events(&session_id)
        .await
        .into_iter()
        .filter(|event| event.seq > seed_head)
        .collect::<Vec<_>>();
    let published = std::iter::from_fn(|| live.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(published).expect("live recovery suffix"),
        serde_json::to_value(&suffix).expect("durable recovery suffix")
    );
    let terminal = suffix.last().expect("failure terminal");
    assert!(matches!(
        terminal.payload.decode_event(),
        Ok(EventPayload::RunState(RunState::Errored))
    ));
    let mut closed = HashSet::new();
    for event in &suffix {
        assert_eq!(
            event.payload.get("provider_request"),
            Some(&source_json),
            "cleanup and terminal retain request 2 without inventing request 3"
        );
        assert!(event.payload.get("provider_finish_reason").is_none());
        if let Ok(EventPayload::Item(ItemEvent::Completed { item_id, item })) =
            event.payload.decode_event()
        {
            assert!(event.seq < terminal.seq, "all items close before terminal");
            assert!(closed.insert(item_id.clone()), "each item closes once");
            if item_id == message_id {
                assert!(matches!(item, TurnItem::AgentMessage { text }
                    if text == "emitted before route loss"));
            } else if item_id == reasoning_id {
                assert!(matches!(item, TurnItem::Reasoning { summary }
                    if summary == "reasoning before route loss"));
            } else {
                assert_eq!(item_id, tool_id);
                assert!(matches!(
                    item,
                    TurnItem::ToolCall {
                        status: ToolStatus::Failed,
                        ..
                    }
                ));
            }
        }
    }
    assert_eq!(closed, HashSet::from([message_id, reasoning_id, tool_id]));
    handle.stop().await.expect("stop recovered actor");
    actor_task.await.expect("recovered actor exits");
}

/// Pin the source records, not a projection synthesized by the client. Losing
/// the request stamp at either the immediate-delta or completion seam must
/// fail even if the final text still happens to look correct.
#[tokio::test]
async fn narrative_items_keep_request_correlation_and_exact_live_journal_parity() {
    let session_id = SessionId::new("journalview-session");
    let run_id = RunId::new("journalview-run");
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("journalview-device"),
        7,
        11,
    )
    .with_stream_delta_coalesce_window(std::time::Duration::ZERO);
    config.turn_ordinal = 9;
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::EmitReasoning {
            text: "prepare the ".into(),
        },
        FakeStep::EmitReasoning {
            text: "requested tool call".into(),
        },
        FakeStep::EmitText {
            text: "I will write ".into(),
        },
        FakeStep::EmitText {
            text: "the event-stream fixture.".into(),
        },
        FakeStep::EmitToolCall {
            call_id: "journalview-call".into(),
            name: "todo_write".into(),
            args: serde_json::json!({"items": []}),
        },
        FakeStep::Finish {
            reason: FinishReason::ToolUse,
        },
        FakeStep::ExpectToolResult {
            call_id: "journalview-call".into(),
        },
        FakeStep::EmitReasoning {
            text: "verify the tool result ".into(),
        },
        FakeStep::EmitReasoning {
            text: "and conclude".into(),
        },
        FakeStep::EmitText { text: "SUC".into() },
        FakeStep::EmitText {
            text: "CESS".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let handle = HarnessActor::spawn(config, provider.clone(), store.clone());
    let mut live = handle.subscribe();

    let outcome = handle
        .submit_committed_turn(SubmitCommittedTurn {
            run_id: run_id.clone(),
            messages: vec![Message::user_text("capture both narrative sides")],
        })
        .await
        .expect("turn accepted")
        .wait()
        .await
        .expect("turn completed");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(provider.requests().len(), 2);

    let journal = store.events(&session_id).await;
    let published = std::iter::from_fn(|| live.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(&published).expect("serialize live stream"),
        serde_json::to_value(&journal).expect("serialize journal"),
        "live and durable envelopes must match including all additive metadata"
    );

    let mut items = BTreeMap::<String, (u64, &'static str, String, bool)>::new();
    let mut delta_count = 0;
    let mut tool_result_count = 0;
    for envelope in &journal {
        let payload = envelope
            .payload
            .decode_event()
            .expect("typed actor payload");
        let (item_id, kind, delta, completed) = match payload {
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::AgentMessage { .. },
            }) => (item_id, "text", None, false),
            EventPayload::Item(ItemEvent::Started {
                item_id,
                item: TurnItem::Reasoning { .. },
            }) => (item_id, "reasoning", None, false),
            EventPayload::Item(ItemEvent::Delta {
                item_id,
                delta: ItemDelta::Text { text },
            }) => (item_id, "text", Some(text.to_owned_string()), false),
            EventPayload::Item(ItemEvent::Delta {
                item_id,
                delta: ItemDelta::Reasoning { text },
            }) => (item_id, "reasoning", Some(text.to_owned_string()), false),
            EventPayload::Item(ItemEvent::Completed {
                item_id,
                item: TurnItem::AgentMessage { text },
            }) => (item_id, "text", Some(text.to_owned_string()), true),
            EventPayload::Item(ItemEvent::Completed {
                item_id,
                item: TurnItem::Reasoning { summary },
            }) => (item_id, "reasoning", Some(summary.to_owned_string()), true),
            EventPayload::ToolResult { .. } => {
                let raw = serde_json::to_value(envelope).expect("serialize tool result");
                assert_eq!(raw["payload"]["provider_request"]["request_ordinal"], 1);
                assert!(raw["payload"].get("provider_finish_reason").is_none());
                tool_result_count += 1;
                continue;
            }
            _ => continue,
        };
        let raw = serde_json::to_value(envelope).expect("serialize narrative event");
        assert_eq!(raw["schema_version"], 1);
        assert!(raw["committed_at_ms"].as_u64().is_some_and(|time| time > 0));
        assert_eq!(envelope.session_id, session_id);
        assert_eq!(envelope.run_id.as_ref(), Some(&run_id));
        assert!(envelope.render.durable);
        let correlation: ProviderRequestAttemptV1 =
            serde_json::from_value(raw["payload"]["provider_request"].clone())
                .expect("every narrative lifecycle event has typed request correlation");
        assert_eq!(correlation.session_id, session_id);
        assert_eq!(correlation.run_id, run_id);
        assert_eq!(correlation.turn_ordinal, 9);
        assert_eq!(correlation.request_kind, ProviderRequestKind::Primary);
        assert!(matches!(correlation.request_ordinal, 1 | 2));
        assert_eq!(
            correlation.turn_id(),
            format!(
                "journalview-session/journalview-run/9/{}",
                correlation.request_ordinal
            )
        );
        let item = items
            .entry(item_id.to_string())
            .or_insert_with(|| (correlation.request_ordinal, kind, String::new(), false));
        assert_eq!(item.0, correlation.request_ordinal);
        assert_eq!(item.1, kind);
        assert!(
            !item.3,
            "no deltas or duplicate completion after an item closes"
        );
        if completed {
            assert_eq!(Some(&item.2), delta.as_ref());
            if correlation.request_ordinal == 1 {
                // ToolCallStart closes this narrative before Finish arrives.
                assert!(raw["payload"].get("provider_finish_reason").is_none());
            } else {
                assert_eq!(raw["payload"]["provider_finish_reason"], "end_turn");
            }
            item.3 = true;
        } else if let Some(delta) = delta {
            item.2.push_str(&delta);
            delta_count += 1;
        }
    }
    let finishes = journal
        .iter()
        .filter(|event| {
            event.payload["event"] == "completed"
                && event.payload["item"]["kind"] == "provider_round_terminal_v1"
        })
        .map(|event| {
            (
                event.payload["provider_request"]["request_ordinal"].clone(),
                event.payload["provider_finish_reason"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finishes,
        vec![
            (serde_json::json!(1), serde_json::json!("tool_use")),
            (serde_json::json!(2), serde_json::json!("end_turn"))
        ]
    );
    assert_eq!(delta_count, 8);
    assert_eq!(tool_result_count, 1);
    assert_eq!(items.len(), 4);
    assert!(items.values().all(|(_, _, _, completed)| *completed));
    let reconstructed = items
        .into_values()
        .map(|(ordinal, kind, text, _)| ((ordinal, kind), text))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        reconstructed,
        BTreeMap::from([
            (
                (1, "text"),
                "I will write the event-stream fixture.".to_owned()
            ),
            (
                (1, "reasoning"),
                "prepare the requested tool call".to_owned()
            ),
            ((2, "text"), "SUCCESS".to_owned()),
            (
                (2, "reasoning"),
                "verify the tool result and conclude".to_owned()
            ),
        ])
    );
    handle.stop().await.expect("stop fixture actor");
}

struct RecoveredChildDispatcher;

#[async_trait]
impl ToolDispatcher for RecoveredChildDispatcher {
    async fn execute(
        &self,
        _run_id: &RunId,
        _item_id: &ItemId,
        _call_id: &str,
        _name: &str,
        _args: serde_json::Value,
        _cancel: &CancelToken,
    ) -> Result<ToolDispatchResult, HaiderError> {
        panic!("recovery must collect the existing child without redispatching it")
    }

    async fn collect_deferred(
        &self,
        ticket: &DeferredTicket,
        _cancel: &CancelToken,
    ) -> Result<DeferredToolResult, HaiderError> {
        Ok(DeferredToolResult {
            report: ChildReport {
                agent: ticket.manifest.agent.clone(),
                summary: "recovered child report".into(),
                verified: ReportVerification::Unverified,
                workspace_revision: None,
            },
            chip: ChipState::Done,
            truncated: false,
            truncation: None,
        })
    }
}

/// A later auxiliary request is deliberately present: restoring the highest
/// request ordinal instead of following the durable tool item misattributes
/// both the recovered result and the completed tool lifecycle.
#[tokio::test]
async fn recovered_child_settlement_keeps_its_source_request_before_next_provider_round() {
    let session_id = SessionId::new("journalview-recovered-session");
    let run_id = RunId::new("journalview-recovered-run");
    let item_id = ItemId::new("journalview-recovered-tool");
    let call_id = "journalview-recovered-call";
    let mut config = HarnessConfig::for_session(
        session_id.clone(),
        DeviceId::new("journalview-recovered-device"),
        7,
        12,
    );
    config.turn_ordinal = 9;
    config.provider_request_ordinal_already_made = 4;
    let provider = Arc::new(FakeProvider::new(vec![
        FakeStep::ExpectToolResult {
            call_id: call_id.into(),
        },
        FakeStep::EmitReasoning {
            text: "use the recovered result".into(),
        },
        FakeStep::EmitText {
            text: "recovered answer".into(),
        },
        FakeStep::Finish {
            reason: FinishReason::EndTurn,
        },
    ]));
    let store = Arc::new(MemoryStore::new());
    let (mut actor, handle) = HarnessActor::new_with_dispatcher(
        config,
        provider.clone(),
        store.clone(),
        Some(Arc::new(RecoveredChildDispatcher)),
    );
    let source_request = ProviderRequestAttemptV1 {
        session_id: session_id.clone(),
        run_id: run_id.clone(),
        turn_ordinal: 9,
        request_ordinal: 2,
        request_kind: ProviderRequestKind::Primary,
    };
    actor.narrative_request = Some(source_request.clone());
    actor
        .commit_item(
            &run_id,
            ItemEvent::Started {
                item_id: item_id.clone(),
                item: TurnItem::ToolCall {
                    call_id: call_id.into(),
                    name: "spawn_subagent".into(),
                    args: serde_json::json!({"task": "resume child"}),
                    status: ToolStatus::InProgress,
                },
            },
        )
        .await
        .expect("original provider tool item is durable");
    actor.provider_finish_reason = Some(FinishReason::ToolUse);
    actor
        .commit_state(
            &run_id,
            RunState::Waiting {
                reason: WaitReason::LocalChild,
            },
        )
        .await
        .expect("original provider finish is durable");
    actor.narrative_request = Some(ProviderRequestAttemptV1 {
        request_ordinal: 4,
        request_kind: ProviderRequestKind::Side,
        ..source_request.clone()
    });
    actor.provider_finish_reason = Some(FinishReason::EndTurn);
    actor
        .commit_closed_item(
            &run_id,
            TurnItem::Extension {
                kind: "journalview_auxiliary_request_fixture".into(),
                data: serde_json::json!({}),
            },
        )
        .await
        .expect("later side request has unrelated durable facts");
    let seed_head = store
        .latest_seq(&session_id)
        .await
        .expect("seed journal head");
    let mut live = handle.subscribe();
    let actor_task = tokio::spawn(actor.run());
    let ticket = DeferredTicket {
        id: "journalview-recovered-ticket".into(),
        manifest: AgentManifest {
            agent: AgentId::new("journalview-recovered-child"),
            role: AgentRole::Subagent,
            task: "resume child".into(),
            callsign: None,
            model_profile: "fake".into(),
            grant: Grant {
                tools: Vec::new(),
                effect_ceiling: Vec::new(),
            },
            budget_tokens: None,
            placement: Placement::Local,
            lease: LeaseId::new("journalview-recovered-lease"),
            fencing_epoch: 1,
            attempt: 0,
            parent: None,
            coordinates: None,
            cli_scope: None,
        },
    };
    let outcome = handle
        .submit_child_wait_turn(SubmitChildWaitTurn {
            run_id: run_id.clone(),
            messages: vec![Message::user_text("resume child")],
            checkpoint: ChildWaitCheckpoint {
                tools: vec![DeferredToolCheckpoint {
                    ticket,
                    tool_item_id: item_id.clone(),
                    call_id: call_id.into(),
                    tool_name: "spawn_subagent".into(),
                    args: serde_json::json!({"task": "resume child"}).to_string(),
                    report_emitted: false,
                    child_result_emitted: false,
                    tool_result_emitted: false,
                    item_completed: false,
                }],
            },
        })
        .await
        .expect("recovered checkpoint accepted")
        .wait()
        .await
        .expect("recovered turn completed");
    assert_eq!(outcome.state, RunState::Done);
    assert_eq!(
        provider.requests().len(),
        1,
        "only the continuation is issued"
    );
    let suffix = store
        .events(&session_id)
        .await
        .into_iter()
        .filter(|event| event.seq > seed_head)
        .collect::<Vec<_>>();
    let published = std::iter::from_fn(|| live.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(published).expect("live recovery events"),
        serde_json::to_value(&suffix).expect("durable recovery events")
    );
    let mut settlements = 0;
    let mut new_narrative = 0;
    for event in &suffix {
        let payload = event.payload.decode_event().expect("typed recovery event");
        let is_settlement = match &payload {
            EventPayload::ToolResult { call_id: found, .. } => found == call_id,
            EventPayload::Item(ItemEvent::Completed { item_id: found, .. }) => found == &item_id,
            _ => false,
        };
        if is_settlement {
            assert_eq!(
                event.payload.get("provider_request"),
                Some(&serde_json::to_value(&source_request).expect("source correlation"))
            );
            assert_eq!(
                event.payload.get("provider_finish_reason"),
                Some(&serde_json::json!("tool_use"))
            );
            settlements += 1;
        }
        if matches!(
            payload,
            EventPayload::Item(ItemEvent::Completed {
                item: TurnItem::AgentMessage { .. } | TurnItem::Reasoning { .. },
                ..
            })
        ) {
            let request = event
                .payload
                .get("provider_request")
                .expect("new request correlation");
            assert_eq!(request["request_ordinal"], 5);
            assert_eq!(request["request_kind"], "primary");
            new_narrative += 1;
        }
    }
    assert_eq!(settlements, 2);
    assert_eq!(new_narrative, 2);
    handle.stop().await.expect("stop recovered actor");
    actor_task.await.expect("recovered actor exits");
}
