//! Deterministic reconstruction of provider messages from committed facts.

use crate::StoreHandle;
use haider_protocol::EventPayload;
use haider_protocol::envelope::PromptRender;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::ids::{AgentId, BranchId, RunId, SessionId};
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_protocol::provider::Block;
use haider_protocol::state::RunState;
use haider_protocol::tool::BoundedResult;
use haider_provider::{Message, MessageRole};
use std::collections::HashMap;

const HISTORY_PAGE: usize = 256;

/// Branch/agent-scoped committed-history compiler.
pub struct PromptHistoryCompiler;

impl PromptHistoryCompiler {
    /// Builds terminal prior conversation plus the current accepted user
    /// message exactly once. Partial output from errored/interrupted runs is
    /// excluded even when individual envelopes requested prompt rendering.
    pub async fn compile(
        store: &dyn StoreHandle,
        session_id: &SessionId,
        branch_id: Option<&BranchId>,
        agent_id: Option<&AgentId>,
        current_run: &RunId,
    ) -> Result<Vec<Message>, HaiderError> {
        let mut envelopes = Vec::new();
        let mut cursor = 0;
        loop {
            let page = store.read(session_id, cursor, HISTORY_PAGE).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |envelope| envelope.seq);
            envelopes.extend(page);
        }

        let mut terminal = HashMap::<RunId, RunState>::new();
        for envelope in &envelopes {
            if envelope.branch_id.as_ref() != branch_id || envelope.agent_id.as_ref() != agent_id {
                continue;
            }
            let Some(run_id) = envelope.run_id.clone() else {
                continue;
            };
            if let Ok(EventPayload::RunState(state)) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            {
                terminal.insert(run_id, state);
            }
        }

        let mut messages = Vec::new();
        let mut pending_tool_results = HashMap::<String, BoundedResult>::new();
        let mut current_user_seen = false;
        for envelope in envelopes {
            if envelope.branch_id.as_ref() != branch_id || envelope.agent_id.as_ref() != agent_id {
                continue;
            }
            let Some(run_id) = envelope.run_id.clone() else {
                continue;
            };
            let is_current = run_id == *current_run;
            if !is_current
                && !terminal
                    .get(&run_id)
                    .is_some_and(|state| *state == RunState::Done)
            {
                continue;
            }
            if envelope.render.prompt == PromptRender::Omit {
                continue;
            }
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload) else {
                continue;
            };
            match payload {
                EventPayload::UserMessage {
                    text, attachments, ..
                } if !is_current || !current_user_seen => {
                    let mut blocks = vec![Block::Text { text }];
                    blocks.extend(attachments.into_iter().map(Block::Attachment));
                    messages.push(Message {
                        role: MessageRole::User,
                        blocks,
                    });
                    if is_current {
                        current_user_seen = true;
                    }
                }
                EventPayload::Item(ItemEvent::Completed { item, .. }) if !is_current => {
                    match item {
                        TurnItem::AgentMessage { text } => {
                            messages.push(Message::assistant(vec![Block::Text { text }]));
                        }
                        TurnItem::ToolCall {
                            call_id,
                            name,
                            args,
                            status: haider_protocol::item::ToolStatus::Completed,
                        } => {
                            messages.push(Message::assistant(vec![Block::ToolCall {
                                call_id: call_id.clone(),
                                name,
                                args,
                            }]));
                            if let Some(result) = pending_tool_results.remove(&call_id) {
                                messages.push(Message::tool_result(
                                    call_id,
                                    result.preview,
                                    result.truncated,
                                ));
                            }
                        }
                        // Reasoning is intentionally excluded: normalized
                        // summaries cannot recreate provider-signed thinking.
                        _ => {}
                    }
                }
                EventPayload::ToolResult { call_id, result } if !is_current => {
                    // Core commits the result before closing the tool item so
                    // cancellation never leaves an unrecorded effect outcome.
                    // Provider history requires the opposite presentation:
                    // assistant tool-call first, then its tool-result.
                    pending_tool_results.insert(call_id, result);
                }
                _ => {}
            }
        }
        if !current_user_seen {
            return Err(HaiderError::new(
                ErrorCode::StoreCorrupt,
                format!("accepted run {current_run} has no committed user message"),
                false,
            ));
        }
        Ok(messages)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::MemoryStore;
    use haider_protocol::DeliveryMode;
    use haider_protocol::envelope::{EventEnvelope, RenderTargets, SCHEMA_VERSION};
    use haider_protocol::ids::{DeviceId, EventId};
    use haider_protocol::item::ToolStatus;

    fn envelope(
        session_id: &SessionId,
        run_id: &RunId,
        event_id: &str,
        payload: EventPayload,
        prompt: PromptRender,
    ) -> haider_protocol::envelope::RawEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION,
            event_id: EventId::new(event_id),
            seq: 0,
            session_id: session_id.clone(),
            branch_id: None,
            run_id: Some(run_id.clone()),
            agent_id: None,
            device_id: DeviceId::new("history-test"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 0,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt,
            },
            payload: serde_json::to_value(payload).expect("payload"),
        }
    }

    /// MUTATION CHECK: emit `ToolResult` immediately in journal order.
    /// Expected failure: the reconstructed provider history begins with a tool
    /// result before its assistant tool call.
    /// Verified by revert on 2026-07-27.
    #[tokio::test]
    async fn tool_result_is_presented_after_its_completed_tool_call() {
        let store = MemoryStore::new();
        let session_id = SessionId::new("history-session");
        let prior = RunId::new("prior-run");
        let current = RunId::new("current-run");
        let mut events = vec![
            envelope(
                &session_id,
                &prior,
                "prior-user",
                EventPayload::UserMessage {
                    text: "read".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
            envelope(
                &session_id,
                &prior,
                "prior-result",
                EventPayload::ToolResult {
                    call_id: "call-1".into(),
                    result: BoundedResult {
                        preview: "contents".into(),
                        truncated: false,
                        artifact: None,
                        cursor: None,
                    },
                },
                PromptRender::Verbatim,
            ),
            envelope(
                &session_id,
                &prior,
                "prior-call",
                EventPayload::Item(ItemEvent::Completed {
                    item_id: haider_protocol::ids::ItemId::new("item-1"),
                    item: TurnItem::ToolCall {
                        call_id: "call-1".into(),
                        name: "fs_read".into(),
                        args: serde_json::json!({"path":"note.txt"}),
                        status: ToolStatus::Completed,
                    },
                }),
                PromptRender::Verbatim,
            ),
            envelope(
                &session_id,
                &prior,
                "prior-done",
                EventPayload::RunState(RunState::Done),
                PromptRender::Omit,
            ),
            envelope(
                &session_id,
                &current,
                "current-user",
                EventPayload::UserMessage {
                    text: "continue".into(),
                    attachments: Vec::new(),
                    mode: DeliveryMode::Queue,
                },
                PromptRender::Verbatim,
            ),
        ];
        StoreHandle::append(&store, &mut events)
            .await
            .expect("append history");
        let messages = PromptHistoryCompiler::compile(&store, &session_id, None, None, &current)
            .await
            .expect("compile");
        let call = messages
            .iter()
            .position(|message| {
                message.blocks.iter().any(
                    |block| matches!(block, Block::ToolCall { call_id, .. } if call_id == "call-1"),
                )
            })
            .expect("tool call");
        let result = messages
            .iter()
            .position(|message| message.tool_result_for("call-1").is_some())
            .expect("tool result");
        assert_eq!(result, call + 1);
    }
}
