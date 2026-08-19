//! Shared transcript projections for JSONL sidecars and instruct-pipe exports.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::history::NodeKind;
use crate::item::{ItemEvent, TurnItem};
use serde::Serialize;

#[derive(Serialize)]
struct TextRow {
    role: &'static str,
    text: String,
    at_ms: u64,
    seq: u64,
}

#[derive(Serialize)]
struct IncompleteRow {
    role: &'static str,
    text: String,
    incomplete: bool,
    interruption: crate::error::ErrorPresentation,
    at_ms: u64,
    seq: u64,
}

#[derive(Serialize)]
struct ErrorRow {
    role: &'static str,
    presentation: crate::error::ErrorPresentation,
    at_ms: u64,
    seq: u64,
}

#[derive(Serialize)]
struct ToolRow {
    role: &'static str,
    name: String,
    summary: String,
    at_ms: u64,
    seq: u64,
}

/// One structured transcript row shared by the sidecar and JSON export.
#[derive(Serialize)]
#[serde(transparent)]
pub struct SidecarRow(SidecarRowKind);

#[derive(Serialize)]
#[serde(untagged)]
enum SidecarRowKind {
    Text(TextRow),
    Incomplete(IncompleteRow),
    Error(ErrorRow),
    Tool(ToolRow),
}

/// Render one durable envelope as one JSONL sidecar row.
///
/// This is also the source of every unmasked JSON export turn within one
/// export window. The sidecar covers the full journal, while a one-shot CLI
/// replay is bounded by its export window; callers can use `--since` to reach
/// the remaining suffix. Payload classification deliberately happens before
/// the more expensive typed decode and payload clone.
#[must_use]
pub fn sidecar_row_line(envelope: &RawEnvelope) -> Option<String> {
    serde_json::to_string(&sidecar_row(envelope)?).ok()
}

/// Build the structured form serialized by [`sidecar_row_line`].
#[must_use]
pub fn sidecar_row(envelope: &RawEnvelope) -> Option<SidecarRow> {
    // Ship-gate round 2: the peek goes DEEP enough that the common case —
    // item Started/Delta/ordinary-Completed, non-projecting node kinds —
    // never pays the full payload clone+decode. Only the exact five
    // qualifying shapes proceed.
    let payload_value = &envelope.payload;
    let type_tag = payload_value
        .get("type")
        .and_then(serde_json::Value::as_str);
    let qualifies = match type_tag {
        Some("node_committed") => payload_value
            .get("kind")
            .and_then(|kind| kind.get("kind"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| matches!(kind, "user_turn" | "assistant_commit" | "tool_exchange")),
        Some("item") => {
            payload_value
                .get("event")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
                && payload_value
                    .get("item")
                    .and_then(|item| item.get("item"))
                    .and_then(serde_json::Value::as_str)
                    == Some("incomplete_agent_message")
        }
        Some("run_failed") => payload_value
            .get("presentation")
            .is_some_and(|presentation| presentation.is_object()),
        _ => false,
    };
    if !qualifies {
        return None;
    }

    let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
    let seq = envelope.seq;
    let at_ms = envelope.committed_at_ms;
    match payload {
        EventPayload::NodeCommitted(node) => match node.kind {
            NodeKind::UserTurn { text, .. } => Some(SidecarRow(SidecarRowKind::Text(TextRow {
                role: "user",
                text,
                at_ms,
                seq,
            }))),
            NodeKind::AssistantCommit { text, .. } => {
                Some(SidecarRow(SidecarRowKind::Text(TextRow {
                    role: "assistant",
                    text,
                    at_ms,
                    seq,
                })))
            }
            NodeKind::ToolExchange { tool, summary, .. } => {
                Some(SidecarRow(SidecarRowKind::Tool(ToolRow {
                    role: "tool",
                    name: tool,
                    summary,
                    at_ms,
                    seq,
                })))
            }
            _ => None,
        },
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::IncompleteAgentMessage { text, interruption },
            ..
        }) => Some(SidecarRow(SidecarRowKind::Incomplete(IncompleteRow {
            role: "assistant",
            text,
            incomplete: true,
            interruption,
            at_ms,
            seq,
        }))),
        EventPayload::RunFailed {
            presentation: Some(presentation),
            ..
        } => Some(SidecarRow(SidecarRowKind::Error(ErrorRow {
            role: "error",
            presentation,
            at_ms,
            seq,
        }))),
        _ => None,
    }
}

/// Wrap one instruct-pipe field and escape characters that would violate the
/// one-line-per-event grammar.
#[must_use]
pub fn escape_pipe_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('|');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out.push('|');
    out
}

/// Render one durable envelope as an instruct-pipe body line.
///
/// Facts outside the transcript projection, including unknown future payloads,
/// return `None` and do not create a body line.
#[must_use]
pub fn pipe_body_line(envelope: &RawEnvelope) -> Option<String> {
    let payload = serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?;
    let seq = envelope.seq;
    let at_ms = envelope.committed_at_ms;
    match payload {
        EventPayload::NodeCommitted(node) => match node.kind {
            NodeKind::UserTurn { text, .. } => {
                Some(format!("U  {seq} {at_ms} {}", escape_pipe_field(&text)))
            }
            NodeKind::AssistantCommit { text, .. } => {
                Some(format!("A  {seq} {at_ms} {}", escape_pipe_field(&text)))
            }
            NodeKind::ToolExchange { tool, summary, .. } => Some(format!(
                "T  {seq} {at_ms} {} {}",
                escape_pipe_field(&tool),
                escape_pipe_field(&summary)
            )),
            _ => None,
        },
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::IncompleteAgentMessage { text, interruption },
            ..
        }) => Some(format!(
            "A! {seq} {at_ms} {} interrupted={}",
            escape_pipe_field(&text),
            escape_pipe_field(&format!("{}: {}", interruption.title, interruption.detail)),
        )),
        EventPayload::RunFailed {
            presentation: Some(presentation),
            ..
        } => Some(format!(
            "E  {seq} {at_ms} {}",
            escape_pipe_field(&format!("{}: {}", presentation.title, presentation.detail)),
        )),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets};
    use crate::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
    use crate::history::TreeNode;
    use crate::ids::{DeviceId, EventId, ItemId, NodeId, SessionId};
    use crate::verify::VerifyVerdict;

    fn envelope(seq: u64, payload: EventPayload) -> RawEnvelope {
        EventEnvelope {
            schema_version: 1,
            event_id: EventId::new(format!("event-{seq}")),
            seq,
            session_id: SessionId::new("session-safe"),
            branch_id: None,
            run_id: None,
            agent_id: None,
            device_id: DeviceId::new("device"),
            authority_epoch: 0,
            worker_generation: 1,
            causation_id: None,
            correlation_id: None,
            committed_at_ms: 1_700_000_000_000 + seq,
            render: RenderTargets {
                ui: true,
                durable: true,
                prompt: PromptRender::Omit,
            },
            payload: serde_json::to_value(payload).expect("serialize payload"),
        }
    }

    fn node(seq: u64, kind: NodeKind) -> RawEnvelope {
        envelope(
            seq,
            EventPayload::NodeCommitted(TreeNode {
                node: NodeId::new(format!("node-{seq}")),
                parent: None,
                kind,
            }),
        )
    }

    fn interruption() -> ErrorPresentation {
        ErrorPresentation::new(
            "hostile",
            "bad | title\\\r",
            "detail\nnext | \\",
            ErrorScope::Turn,
            [ErrorAction::Retry],
        )
    }

    #[test]
    fn hostile_pipe_lines_pin_the_five_way_projection_and_line_law() {
        let hostile = "first | field\\\r\nsecond";
        let events = [
            node(
                1,
                NodeKind::UserTurn {
                    text: hostile.into(),
                    attachments: Vec::new(),
                },
            ),
            node(
                2,
                NodeKind::AssistantCommit {
                    text: hostile.into(),
                    verdict: VerifyVerdict::Unverified,
                },
            ),
            node(
                3,
                NodeKind::ToolExchange {
                    tool: "bash | hostile\\\r\nname".into(),
                    summary: hostile.into(),
                    artifact: None,
                },
            ),
            envelope(
                4,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new("incomplete"),
                    item: TurnItem::IncompleteAgentMessage {
                        text: hostile.into(),
                        interruption: interruption(),
                    },
                }),
            ),
            envelope(
                5,
                EventPayload::RunFailed {
                    code: ErrorCode::ProviderError,
                    message: "legacy".into(),
                    retryable: true,
                    presentation: Some(interruption()),
                },
            ),
        ];
        let lines: Vec<String> = events.iter().filter_map(pipe_body_line).collect();
        assert_eq!(
            lines,
            [
                "U  1 1700000000001 |first \\| field\\\\\\nsecond|",
                "A  2 1700000000002 |first \\| field\\\\\\nsecond|",
                "T  3 1700000000003 |bash \\| hostile\\\\\\nname| |first \\| field\\\\\\nsecond|",
                "A! 4 1700000000004 |first \\| field\\\\\\nsecond| interrupted=|bad \\| title\\\\ : detail\\nnext \\| \\\\|",
                "E  5 1700000000005 |bad \\| title\\\\ : detail\\nnext \\| \\\\|",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains(['\n', '\r'])));
        assert_eq!(escape_pipe_field("a\\|\r\nb"), "|a\\\\\\|\\nb|");
    }
}
