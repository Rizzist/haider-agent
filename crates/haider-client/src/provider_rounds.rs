//! Deterministic request-correlated narrative projection of the durable ledger.
//!
//! This is derived container metadata, never an event decoration. Old journals
//! without request coordinates remain lossless in `events`; we do not guess a
//! physical request from event adjacency or fabricate unavailable reasoning.

use crate::headless::HeadlessRunEvents;
use haider_rpc::haider_protocol::EventPayload;
use haider_rpc::haider_protocol::cache::ProviderRequestAttemptV1;
use haider_rpc::haider_protocol::envelope::RawEnvelope;
use haider_rpc::haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_rpc::haider_protocol::reply::{ReplyArenaWriter, ReplyText};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io;

#[derive(Debug, Serialize)]
pub struct NarrativeItem {
    pub item_id: String,
    pub text: ReplyText,
    pub completed: bool,
    pub first_seq: u64,
    pub last_seq: u64,
    pub committed_at_ms: u64,
    pub schema_version: u32,
    #[serde(skip)]
    writer: Option<ReplyArenaWriter>,
}

#[derive(Debug, Serialize)]
pub struct ProviderRound {
    #[serde(flatten)]
    pub request: ProviderRequestAttemptV1,
    pub emitted_text: Vec<NarrativeItem>,
    pub reasoning_summary: Vec<NarrativeItem>,
    pub tool_calls: Vec<Value>,
    pub results: Vec<Value>,
    /// Provider finish reason when emitted; otherwise the durable failure or
    /// retry state. Null means the journal has no terminal cause for this round.
    pub terminal_cause: Option<String>,
    #[serde(skip)]
    has_provider_terminal: bool,
}

/// One projection for both `--output json` and `run --replay`.
pub fn provider_rounds(events: &HeadlessRunEvents) -> io::Result<Vec<ProviderRound>> {
    project_provider_rounds(events.iter()?)
}

pub fn project_provider_rounds(
    events: impl IntoIterator<Item = io::Result<RawEnvelope>>,
) -> io::Result<Vec<ProviderRound>> {
    let mut rounds = BTreeMap::<(u64, u64), ProviderRound>::new();
    let mut item_requests = BTreeMap::new();
    for envelope in events {
        let envelope = envelope?;
        let payload = &envelope.payload;
        let mut request = payload.get("provider_request").and_then(|value| {
            serde_json::from_value::<ProviderRequestAttemptV1>(value.clone()).ok()
        });
        // The pre-dispatch marker also records attempts that emitted no content.
        if request.is_none()
            && let Ok(EventPayload::Item(ItemEvent::Completed { item, .. })) =
                payload.decode_event()
        {
            request = ProviderRequestAttemptV1::try_from_extension_item(&item)
                .ok()
                .flatten();
        }
        let Some(request) = request else { continue };
        if request.session_id != envelope.session_id
            || envelope.run_id.as_ref() != Some(&request.run_id)
            || !request.coordinates_valid()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid narrative request correlation",
            ));
        }
        let key = (request.turn_ordinal, request.request_ordinal);
        let prior_request_item = payload
            .get("item_id")
            .and_then(Value::as_str)
            .is_some_and(|item_id| *item_requests.entry(item_id.to_owned()).or_insert(key) != key);
        let round = rounds
            .entry((request.turn_ordinal, request.request_ordinal))
            .or_insert_with(|| ProviderRound {
                request,
                emitted_text: Vec::new(),
                reasoning_summary: Vec::new(),
                tool_calls: Vec::new(),
                results: Vec::new(),
                terminal_cause: None,
                has_provider_terminal: false,
            });
        if let Some(reason) = payload
            .get("provider_finish_reason")
            .or_else(|| payload.get("provider_terminal_cause"))
            .and_then(Value::as_str)
        {
            round.terminal_cause = Some(reason.to_owned());
            round.has_provider_terminal = true;
        } else if !round.has_provider_terminal && payload.type_tag() == Some("run_state") {
            round.terminal_cause = match payload.get("state").and_then(Value::as_str) {
                Some("retrying") => Some("retrying".into()),
                Some("errored") => Some("error".into()),
                Some("cancelled") => Some("cancelled".into()),
                _ => round.terminal_cause.take(),
            };
        }
        match payload.decode_event() {
            Ok(EventPayload::Item(ItemEvent::Delta { item_id, delta })) => match delta {
                ItemDelta::Text { text } => append_narrative(
                    &mut round.emitted_text,
                    item_id.as_str(),
                    text,
                    false,
                    true,
                    &envelope,
                ),
                ItemDelta::Reasoning { text } => append_narrative(
                    &mut round.reasoning_summary,
                    item_id.as_str(),
                    text,
                    false,
                    true,
                    &envelope,
                ),
                _ => {}
            },
            Ok(EventPayload::Item(
                ItemEvent::Started { item_id, item } | ItemEvent::Completed { item_id, item },
            )) => {
                let completed = payload.get("event").and_then(Value::as_str) == Some("completed");
                match item {
                    TurnItem::AgentMessage { text } => append_narrative(
                        &mut round.emitted_text,
                        item_id.as_str(),
                        if prior_request_item {
                            ReplyText::default()
                        } else {
                            text
                        },
                        completed,
                        false,
                        &envelope,
                    ),
                    TurnItem::IncompleteAgentMessage { text, .. } => append_narrative(
                        &mut round.emitted_text,
                        item_id.as_str(),
                        if prior_request_item {
                            ReplyText::default()
                        } else {
                            text
                        },
                        false,
                        false,
                        &envelope,
                    ),
                    TurnItem::Reasoning { summary } => append_narrative(
                        &mut round.reasoning_summary,
                        item_id.as_str(),
                        if prior_request_item {
                            ReplyText::default()
                        } else {
                            summary
                        },
                        completed,
                        false,
                        &envelope,
                    ),
                    TurnItem::ToolCall { ref call_id, .. } => {
                        let item = serde_json::to_value(&item).map_err(io::Error::other)?;
                        if let Some(existing) = round
                            .tool_calls
                            .iter_mut()
                            .find(|value| value["call_id"].as_str() == Some(call_id.as_str()))
                        {
                            *existing = item;
                        } else {
                            round.tool_calls.push(item);
                        }
                    }
                    _ => {}
                }
            }
            Ok(EventPayload::ToolResult { call_id, result }) => {
                round
                    .results
                    .push(serde_json::json!({"call_id": call_id, "result": result}));
            }
            _ => {}
        }
    }
    for round in rounds.values_mut() {
        for item in round
            .emitted_text
            .iter_mut()
            .chain(&mut round.reasoning_summary)
        {
            if let Some(writer) = item.writer.take() {
                item.text = writer.seal();
            }
        }
    }
    Ok(rounds.into_values().collect())
}

fn append_narrative(
    items: &mut Vec<NarrativeItem>,
    item_id: &str,
    text: ReplyText,
    completed: bool,
    delta: bool,
    envelope: &RawEnvelope,
) {
    if let Some(item) = items.iter_mut().find(|item| item.item_id == item_id) {
        if delta {
            let _ = item
                .writer
                .get_or_insert_with(ReplyArenaWriter::new)
                .append_shared(&text);
        } else if item.writer.is_none() {
            item.text = text;
        }
        item.completed |= completed;
        item.last_seq = envelope.seq;
        item.committed_at_ms = envelope.committed_at_ms;
    } else {
        items.push(NarrativeItem {
            item_id: item_id.to_owned(),
            text: text.clone(),
            completed,
            first_seq: envelope.seq,
            last_seq: envelope.seq,
            committed_at_ms: envelope.committed_at_ms,
            schema_version: envelope.schema_version,
            writer: delta.then(|| {
                let mut writer = ReplyArenaWriter::new();
                let _ = writer.append_shared(&text);
                writer
            }),
        });
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: u64, ordinal: u64, mut payload: Value) -> RawEnvelope {
        payload["provider_request"] = json!({
            "session_id": "s", "run_id": "r", "turn_ordinal": 1,
            "request_ordinal": ordinal, "request_kind": "primary",
        });
        serde_json::from_value(json!({
            "schema_version": 1, "event_id": format!("e{seq}"), "seq": seq,
            "session_id": "s", "run_id": "r", "device_id": "d",
            "authority_epoch": 1, "worker_generation": 1, "committed_at_ms": seq,
            "render": {"ui": true, "durable": true, "prompt": "omit"}, "payload": payload,
        }))
        .expect("fixture envelope")
    }

    #[test]
    fn journalview_unsuccessful_summary_finish_keeps_text_incomplete() {
        for reason in [
            "cancelled",
            "error",
            "max_tokens",
            "refusal",
            "tool_use",
            "pause_turn",
        ] {
            let events = [event(
                1,
                1,
                json!({
                    "type": "item", "event": "completed", "item_id": "summary",
                    "item": {"item": "incomplete_agent_message", "text": "partial summary",
                        "interruption": haider_rpc::haider_protocol::error::ErrorPresentation::default()},
                    "provider_purpose": "compaction", "provider_finish_reason": reason,
                }),
            )];
            let rounds = project_provider_rounds(events.into_iter().map(Ok)).expect("rounds");
            assert_eq!(rounds.len(), 1);
            assert_eq!(
                rounds[0].emitted_text[0].text.to_owned_string(),
                "partial summary"
            );
            assert!(!rounds[0].emitted_text[0].completed, "{reason}");
            assert_eq!(rounds[0].terminal_cause.as_deref(), Some(reason));
        }
    }

    #[test]
    fn journalview_retry_completion_does_not_duplicate_an_earlier_requests_text() {
        let events = vec![
            event(
                1,
                1,
                json!({"type":"item", "event":"delta", "item_id":"i", "delta":{"delta":"text", "text":"prefix"}}),
            ),
            event(2, 1, json!({"type":"run_state", "state":"retrying"})),
            event(
                3,
                2,
                json!({"type":"item", "event":"completed", "item_id":"i", "item":{"item":"agent_message", "text":"prefix"}, "provider_finish_reason":"end_turn"}),
            ),
        ];
        let rounds = project_provider_rounds(events.into_iter().map(Ok)).expect("rounds");
        assert_eq!(rounds[0].emitted_text[0].text.to_owned_string(), "prefix");
        assert!(!rounds[0].emitted_text[0].completed);
        assert_eq!(rounds[1].emitted_text[0].text.to_owned_string(), "");
        assert!(rounds[1].emitted_text[0].completed);
        assert_eq!(rounds[1].terminal_cause.as_deref(), Some("end_turn"));
    }

    #[test]
    fn journalview_retry_cancellation_and_stream_failure_have_honest_causes() {
        let events = vec![
            event(1, 1, json!({"type":"run_state", "state":"retrying"})),
            event(2, 1, json!({"type":"run_state", "state":"cancelled"})),
            event(
                3,
                2,
                json!({"type":"item", "event":"delta", "item_id":"j", "delta":{"delta":"reasoning", "text":"emitted summary"}}),
            ),
            event(4, 2, json!({"type":"run_state", "state":"errored"})),
        ];
        let rounds = project_provider_rounds(events.into_iter().map(Ok)).expect("rounds");
        assert_eq!(rounds[0].terminal_cause.as_deref(), Some("cancelled"));
        assert_eq!(rounds[1].terminal_cause.as_deref(), Some("error"));
        assert_eq!(
            rounds[1].reasoning_summary[0].text.to_owned_string(),
            "emitted summary"
        );
        assert!(!rounds[1].reasoning_summary[0].completed);
    }

    #[test]
    fn journalview_large_delta_projection_uses_shared_arena_and_replays_identically() {
        let chunk = "x".repeat(4096);
        let events: Vec<_> = (1..=256).map(|seq| event(seq, 1, json!({
            "type":"item", "event":"delta", "item_id":"i", "delta":{"delta":"text", "text":chunk},
        }))).collect();
        let live = project_provider_rounds(events.clone().into_iter().map(Ok)).expect("live");
        let reloaded: Vec<RawEnvelope> =
            serde_json::from_slice(&serde_json::to_vec(&events).expect("journal bytes"))
                .expect("journal reload");
        let replay = project_provider_rounds(reloaded.into_iter().map(Ok)).expect("replay");
        assert_eq!(live[0].emitted_text[0].text.len(), 4096 * 256);
        assert!(
            live[0].emitted_text[0].text.arena_digest().is_some(),
            "projection is a sealed arena, not repeatedly flattened Strings"
        );
        assert_eq!(
            serde_json::to_value(live).expect("live JSON"),
            serde_json::to_value(replay).expect("replay JSON")
        );
    }

    #[test]
    fn journalview_legacy_and_unknown_events_preserve_raw_without_invented_rounds() {
        let mut legacy = event(1, 1, json!({"type":"future_kind", "value":"unknown"}));
        legacy
            .payload
            .as_object_mut()
            .expect("object")
            .remove("provider_request");
        let before = serde_json::to_vec(&legacy).expect("before");
        assert!(
            project_provider_rounds([Ok(legacy.clone())])
                .expect("legacy")
                .is_empty()
        );
        assert_eq!(serde_json::to_vec(&legacy).expect("after"), before);
        let mut future = event(
            2,
            2,
            json!({"type":"item", "event":"completed", "item_id":"future", "item":{"item":"agent_message", "text":"still preserved"}}),
        );
        future.payload["provider_request"]["request_kind"] = json!("future_request_kind");
        let before = serde_json::to_vec(&future).expect("future bytes");
        assert!(
            project_provider_rounds([Ok(future.clone())])
                .expect("future metadata is additive")
                .is_empty()
        );
        assert_eq!(
            serde_json::to_vec(&future).expect("future unchanged"),
            before
        );
    }
}
