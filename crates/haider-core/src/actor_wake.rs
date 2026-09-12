//! Provider-neutral repair of the unfinished tool exchange before boundary input.
//! Completed history, cache prefixes and peer speaker boundaries stay intact.

use haider_protocol::provider::Block;
use haider_provider::{Message, MessageRole};

pub(super) const HELD_TOOL_RESULT: &str =
    "held before execution for a user subturn; revise or confirm the tool call";

pub(super) fn input(text: String) -> Message {
    Message::user_text(if text.trim().is_empty() {
        "[Subturn input contained no visible text.]".to_owned()
    } else {
        text
    })
}

pub(super) struct TailRemap {
    insert_at: usize,
    inserted: usize,
}

impl TailRemap {
    pub(super) fn boundary(&self, boundary: usize) -> usize {
        boundary
            + if boundary >= self.insert_at {
                self.inserted
            } else {
                0
            }
    }
}

/// A monitor/menu/committed wake is ordinary user text, never a second result
/// for the registration call. Finish missing pairs before that input. Actual
/// results (including failures) are authoritative and are never replaced.
/// Adjacent user messages are legal on the Messages API: it combines their
/// blocks in order. Keeping them separate in the IR also preserves Chat's
/// dedicated tool role and the compiler's cache/speaker boundaries.
pub(super) fn normalize_tail(messages: &mut Vec<Message>) -> TailRemap {
    let start = messages
        .iter()
        .rposition(|message| message.role == MessageRole::Assistant)
        .map_or(0, |index| index + 1);
    let assistant_start = messages[..start]
        .iter()
        .rposition(|message| message.role != MessageRole::Assistant)
        .map_or(0, |index| index + 1);
    let mut missing = Vec::new();
    for message in &messages[assistant_start..start] {
        for block in &message.blocks {
            if let Block::ToolCall { call_id, .. } = block
                && !messages[start..].iter().flat_map(|message| &message.blocks).any(|block| {
                    matches!(block, Block::ToolResult { call_id: found, .. } if found == call_id)
                })
            {
                missing.push(Message::tool_result(call_id.clone(),
                    "No result is available for this call at the input boundary; do not assume execution succeeded.",
                    false));
            }
        }
    }
    // Redaction or an empty input must not create an empty API text block.
    for message in &mut messages[start..] {
        for block in &mut message.blocks {
            if let Block::Text { text } = block
                && text.is_blank()
            {
                *text = "[Input contained no visible text.]".into();
            }
        }
    }
    let inserted = missing.len();
    messages.splice(start..start, missing);
    TailRemap {
        insert_at: start,
        inserted,
    }
}
