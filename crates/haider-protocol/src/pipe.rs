//! Native instruct-pipe body rendering shared by export and daemon projections.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::history::NodeKind;
use crate::item::{ItemEvent, TurnItem};

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
                "T  {seq} {at_ms} {tool} {}",
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
                    tool: "bash".into(),
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
                "T  3 1700000000003 bash |first \\| field\\\\\\nsecond|",
                "A! 4 1700000000004 |first \\| field\\\\\\nsecond| interrupted=|bad \\| title\\\\ : detail\\nnext \\| \\\\|",
                "E  5 1700000000005 |bad \\| title\\\\ : detail\\nnext \\| \\\\|",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains(['\n', '\r'])));
        assert_eq!(escape_pipe_field("a\\|\r\nb"), "|a\\\\\\|\\nb|");
    }
}
