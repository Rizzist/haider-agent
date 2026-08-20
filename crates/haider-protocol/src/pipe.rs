//! Shared transcript projections for JSONL sidecars and instruct-pipe exports.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::history::NodeKind;
use crate::item::{ItemEvent, TurnItem};
use crate::tool::BoundedResult;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

/// Maximum number of Unicode scalar values carried by a cold-history tool
/// argument or result preview.
pub const TOOL_PREVIEW_CHARS: usize = 160;
const MAX_PENDING_TOOL_RESULTS: usize = 1_024;
type ToolJoinKey = (Option<String>, Option<String>, String);

/// Full journal facts joined to one committed tool-exchange node.
#[derive(Debug, Clone)]
pub struct ToolExchangeJoin {
    pub call_id: String,
    pub args: serde_json::Value,
    pub result: Option<BoundedResult>,
    pub tool_call_seq: u64,
    pub tool_result_seq: Option<u64>,
}

impl ToolExchangeJoin {
    #[must_use]
    pub fn args_preview(&self) -> Option<String> {
        args_preview(&self.args)
    }

    #[must_use]
    pub fn result_preview(&self) -> Option<String> {
        self.result.as_ref().and_then(result_preview)
    }
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    seq: u64,
    name: String,
    call_id: String,
    args: serde_json::Value,
    branch_id: Option<String>,
    run_id: Option<String>,
}

/// Stateful, bounded join over the durable transcript facts. A tool node is
/// paired only with the immediately preceding completed call item; the
/// call-id then resolves its independently committed result.
#[derive(Debug, Clone, Default)]
pub struct TranscriptJoiner {
    previous_tool_call: Option<PendingToolCall>,
    pending_results: HashMap<ToolJoinKey, (u64, BoundedResult)>,
    result_order: VecDeque<ToolJoinKey>,
}

#[derive(Default)]
pub struct TranscriptProjector {
    joiner: TranscriptJoiner,
    buffered: VecDeque<BufferedRow>,
}

struct BufferedRow {
    row: SidecarRow,
    unresolved_tool: Option<ToolJoinKey>,
    remaining_facts: usize,
}

impl TranscriptProjector {
    /// Rebuild join state through a cursor without projecting rows that are
    /// already durable. Rows after that cursor still enter through [`Self::push`]
    /// and may wait for a later result as usual.
    pub fn prewarm(&mut self, envelope: &RawEnvelope) {
        let _ = self.joiner.observe(envelope);
    }

    /// Project one ordered envelope. Rows following an unresolved tool stay
    /// buffered so a provider result committed after its node can be joined
    /// without reordering the transcript. The fact bound makes corruption or
    /// an absent result degrade to an args-only row with bounded memory.
    pub fn push(&mut self, envelope: &RawEnvelope) -> Vec<SidecarRow> {
        for buffered in &mut self.buffered {
            if buffered.unresolved_tool.is_some() {
                buffered.remaining_facts = buffered.remaining_facts.saturating_sub(1);
                if buffered.remaining_facts == 0 {
                    buffered.unresolved_tool = None;
                }
            }
        }

        let result = matching_result(envelope);
        if let Some(projection) = sidecar_projection(&mut self.joiner, envelope) {
            self.buffered.push_back(BufferedRow {
                row: projection.row,
                unresolved_tool: projection.unresolved_tool,
                remaining_facts: MAX_PENDING_TOOL_RESULTS,
            });
        }
        if let Some((key, result)) = result
            && let Some(buffered) = self
                .buffered
                .iter_mut()
                .find(|buffered| buffered.unresolved_tool.as_ref() == Some(&key))
        {
            buffered.row.set_result_preview(result_preview(&result));
            buffered.unresolved_tool = None;
            self.joiner.remove_result(&key);
        }

        let mut rows = Vec::new();
        while self
            .buffered
            .front()
            .is_some_and(|buffered| buffered.unresolved_tool.is_none())
        {
            if let Some(buffered) = self.buffered.pop_front() {
                rows.push(buffered.row);
            }
        }
        rows
    }

    /// Flush final unresolved rows without fabricating results.
    pub fn finish(&mut self) -> Vec<SidecarRow> {
        self.buffered
            .drain(..)
            .map(|buffered| buffered.row)
            .collect()
    }

    /// Earliest row withheld from durable sidecar coverage.
    #[must_use]
    pub fn blocked_seq(&self) -> Option<u64> {
        self.buffered.front().map(|buffered| buffered.row.seq())
    }
}

fn matching_result(envelope: &RawEnvelope) -> Option<(ToolJoinKey, BoundedResult)> {
    // Type-tag peek: `push` calls this for EVERY envelope, and only a
    // `tool_result` can match. Serde's internal tag makes the peek exact,
    // so everything else skips the payload deep-clone + full decode.
    if envelope
        .payload
        .get("type")
        .and_then(serde_json::Value::as_str)
        != Some("tool_result")
    {
        return None;
    }
    let EventPayload::ToolResult { call_id, result } =
        serde_json::from_value::<EventPayload>(envelope.payload.clone()).ok()?
    else {
        return None;
    };
    Some((
        (
            envelope
                .branch_id
                .as_ref()
                .map(|branch| branch.as_str().to_owned()),
            envelope.run_id.as_ref().map(|run| run.as_str().to_owned()),
            call_id,
        ),
        result,
    ))
}

impl TranscriptJoiner {
    #[must_use]
    pub fn observe(&mut self, envelope: &RawEnvelope) -> Option<ToolExchangeJoin> {
        let payload = &envelope.payload;
        let type_tag = payload.get("type").and_then(serde_json::Value::as_str);
        let relevant = match type_tag {
            Some("tool_result") => true,
            Some("item") => {
                payload.get("event").and_then(serde_json::Value::as_str) == Some("completed")
                    && payload
                        .get("item")
                        .and_then(|item| item.get("item"))
                        .and_then(serde_json::Value::as_str)
                        == Some("tool_call")
            }
            Some("node_committed") => {
                payload
                    .get("kind")
                    .and_then(|kind| kind.get("kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some("tool_exchange")
            }
            _ => false,
        };
        if !relevant {
            self.previous_tool_call = None;
            return None;
        }
        let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
            self.previous_tool_call = None;
            return None;
        };
        match payload {
            EventPayload::ToolResult { call_id, result } => {
                self.previous_tool_call = None;
                self.insert_result(
                    envelope
                        .branch_id
                        .as_ref()
                        .map(|branch| branch.as_str().to_owned()),
                    envelope.run_id.as_ref().map(|run| run.as_str().to_owned()),
                    call_id,
                    envelope.seq,
                    result,
                );
                None
            }
            EventPayload::Item(ItemEvent::Completed {
                item:
                    TurnItem::ToolCall {
                        call_id,
                        name,
                        args,
                        ..
                    },
                ..
            }) => {
                self.previous_tool_call = Some(PendingToolCall {
                    seq: envelope.seq,
                    name,
                    call_id,
                    args,
                    branch_id: envelope
                        .branch_id
                        .as_ref()
                        .map(|branch| branch.as_str().to_owned()),
                    run_id: envelope.run_id.as_ref().map(|run| run.as_str().to_owned()),
                });
                None
            }
            EventPayload::NodeCommitted(node) => {
                let NodeKind::ToolExchange { tool, .. } = node.kind else {
                    self.previous_tool_call = None;
                    return None;
                };
                let call = self.previous_tool_call.take().filter(|call| {
                    call.seq.checked_add(1) == Some(envelope.seq) && call.name == tool
                })?;
                let result_key = (call.branch_id, call.run_id, call.call_id.clone());
                let result = self.pending_results.remove(&result_key);
                if result.is_some() {
                    self.result_order.retain(|key| key != &result_key);
                }
                Some(ToolExchangeJoin {
                    call_id: call.call_id,
                    args: call.args,
                    result: result.as_ref().map(|(_, result)| result.clone()),
                    tool_call_seq: call.seq,
                    tool_result_seq: result.map(|(seq, _)| seq),
                })
            }
            _ => {
                self.previous_tool_call = None;
                None
            }
        }
    }

    fn insert_result(
        &mut self,
        branch_id: Option<String>,
        run_id: Option<String>,
        call_id: String,
        seq: u64,
        result: BoundedResult,
    ) {
        let key = (branch_id, run_id, call_id);
        if !self.pending_results.contains_key(&key) {
            self.result_order.push_back(key.clone());
        }
        self.pending_results.insert(key, (seq, result));
        while self.pending_results.len() > MAX_PENDING_TOOL_RESULTS {
            let Some(oldest) = self.result_order.pop_front() else {
                break;
            };
            self.pending_results.remove(&oldest);
        }
    }

    fn remove_result(&mut self, key: &ToolJoinKey) {
        self.pending_results.remove(key);
        self.result_order.retain(|candidate| candidate != key);
    }
}

/// Normalize a preview to one line and cap it by characters, never bytes.
#[must_use]
pub fn normalize_tool_preview(raw: &str) -> Option<String> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.chars().take(TOOL_PREVIEW_CHARS).collect())
    }
}

/// Prefer the argument a human scans for, falling back to compact JSON.
#[must_use]
pub fn args_preview(args: &serde_json::Value) -> Option<String> {
    if let Some(object) = args.as_object() {
        for key in ["url", "path", "cmd", "pattern"] {
            let Some(value) = object.get(key) else {
                continue;
            };
            let raw = match value {
                serde_json::Value::Null => continue,
                serde_json::Value::String(value) => value.clone(),
                value => serde_json::to_string(value).ok()?,
            };
            if let Some(preview) = normalize_tool_preview(&raw) {
                return Some(preview);
            }
        }
    }
    normalize_tool_preview(&serde_json::to_string(args).ok()?)
}

/// Prefer a result's bounded preview and fall back to its typed reason.
#[must_use]
pub fn result_preview(result: &BoundedResult) -> Option<String> {
    normalize_tool_preview(&result.preview)
        .or_else(|| result.reason.as_deref().and_then(normalize_tool_preview))
}

#[derive(Serialize)]
struct TextRow {
    role: &'static str,
    text: String,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    ordinal: u32,
}

#[derive(Serialize)]
struct IncompleteRow {
    role: &'static str,
    text: String,
    incomplete: bool,
    interruption: crate::error::ErrorPresentation,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    ordinal: u32,
}

#[derive(Serialize)]
struct ErrorRow {
    role: &'static str,
    presentation: crate::error::ErrorPresentation,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    ordinal: u32,
}

#[derive(Serialize)]
struct ToolRow {
    role: &'static str,
    name: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    args_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_preview: Option<String>,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    ordinal: u32,
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

impl SidecarRow {
    fn set_result_preview(&mut self, preview: Option<String>) {
        if let SidecarRowKind::Tool(row) = &mut self.0 {
            row.result_preview = preview;
        }
    }

    /// Journal sequence that produced this transcript row.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match &self.0 {
            SidecarRowKind::Text(row) => row.seq,
            SidecarRowKind::Incomplete(row) => row.seq,
            SidecarRowKind::Error(row) => row.seq,
            SidecarRowKind::Tool(row) => row.seq,
        }
    }

    /// Render this structured transcript row in the instruct-pipe grammar.
    #[must_use]
    pub fn pipe_body_line(&self) -> String {
        match &self.0 {
            SidecarRowKind::Text(row) if row.role == "user" => {
                format!(
                    "U  {} {} {}",
                    row.seq,
                    row.at_ms,
                    escape_pipe_field(&row.text)
                )
            }
            SidecarRowKind::Text(row) => {
                format!(
                    "A  {} {} {}",
                    row.seq,
                    row.at_ms,
                    escape_pipe_field(&row.text)
                )
            }
            SidecarRowKind::Incomplete(row) => format!(
                "A! {} {} {} interrupted={}",
                row.seq,
                row.at_ms,
                escape_pipe_field(&row.text),
                escape_pipe_field(&format!(
                    "{}: {}",
                    row.interruption.title, row.interruption.detail
                )),
            ),
            SidecarRowKind::Error(row) => format!(
                "E  {} {} {}",
                row.seq,
                row.at_ms,
                escape_pipe_field(&format!(
                    "{}: {}",
                    row.presentation.title, row.presentation.detail
                )),
            ),
            SidecarRowKind::Tool(row) => {
                let mut line = format!(
                    "T  {} {} {} {}",
                    row.seq,
                    row.at_ms,
                    escape_pipe_field(&row.name),
                    escape_pipe_field(&row.summary)
                );
                if let Some(args) = &row.args_preview {
                    line.push_str(" args=");
                    line.push_str(&escape_pipe_field(args));
                }
                if let Some(result) = &row.result_preview {
                    line.push_str(" result=");
                    line.push_str(&escape_pipe_field(result));
                }
                line
            }
        }
    }
}

struct SidecarProjection {
    row: SidecarRow,
    unresolved_tool: Option<ToolJoinKey>,
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

/// Stateful form of [`sidecar_row_line`] used by journal readers that can
/// resolve the adjacent tool-call/result facts.
#[must_use]
pub fn sidecar_row_line_with(
    joiner: &mut TranscriptJoiner,
    envelope: &RawEnvelope,
) -> Option<String> {
    serde_json::to_string(&sidecar_row_with(joiner, envelope)?).ok()
}

/// Build the structured form serialized by [`sidecar_row_line`].
#[must_use]
pub fn sidecar_row(envelope: &RawEnvelope) -> Option<SidecarRow> {
    sidecar_row_with(&mut TranscriptJoiner::default(), envelope)
}

/// Stateful form of [`sidecar_row`] that adds joined tool previews when the
/// relevant facts are present in the same ordered journal read.
#[must_use]
pub fn sidecar_row_with(
    joiner: &mut TranscriptJoiner,
    envelope: &RawEnvelope,
) -> Option<SidecarRow> {
    sidecar_projection(joiner, envelope).map(|projection| projection.row)
}

fn sidecar_projection(
    joiner: &mut TranscriptJoiner,
    envelope: &RawEnvelope,
) -> Option<SidecarProjection> {
    let tool_join = joiner.observe(envelope);
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
    let branch_id = envelope
        .branch_id
        .as_ref()
        .map(|branch_id| branch_id.as_str().to_owned());
    match payload {
        EventPayload::NodeCommitted(node) => match node.kind {
            NodeKind::UserTurn { text, .. } => Some(SidecarProjection {
                row: SidecarRow(SidecarRowKind::Text(TextRow {
                    role: "user",
                    text,
                    at_ms,
                    seq,
                    branch_id,
                    ordinal: 0,
                })),
                unresolved_tool: None,
            }),
            NodeKind::AssistantCommit { text, .. } => Some(SidecarProjection {
                row: SidecarRow(SidecarRowKind::Text(TextRow {
                    role: "assistant",
                    text,
                    at_ms,
                    seq,
                    branch_id,
                    ordinal: 0,
                })),
                unresolved_tool: None,
            }),
            NodeKind::ToolExchange { tool, summary, .. } => Some(SidecarProjection {
                unresolved_tool: tool_join.as_ref().and_then(|join| {
                    join.result.is_none().then(|| {
                        (
                            branch_id.clone(),
                            envelope.run_id.as_ref().map(|run| run.as_str().to_owned()),
                            join.call_id.clone(),
                        )
                    })
                }),
                row: SidecarRow(SidecarRowKind::Tool(ToolRow {
                    role: "tool",
                    name: tool,
                    summary,
                    args_preview: tool_join.as_ref().and_then(ToolExchangeJoin::args_preview),
                    result_preview: tool_join
                        .as_ref()
                        .and_then(ToolExchangeJoin::result_preview),
                    at_ms,
                    seq,
                    branch_id,
                    ordinal: 0,
                })),
            }),
            _ => None,
        },
        EventPayload::Item(ItemEvent::Completed {
            item: TurnItem::IncompleteAgentMessage { text, interruption },
            ..
        }) => Some(SidecarProjection {
            row: SidecarRow(SidecarRowKind::Incomplete(IncompleteRow {
                role: "assistant",
                text,
                incomplete: true,
                interruption,
                at_ms,
                seq,
                branch_id,
                ordinal: 0,
            })),
            unresolved_tool: None,
        }),
        EventPayload::RunFailed {
            presentation: Some(presentation),
            ..
        } => Some(SidecarProjection {
            row: SidecarRow(SidecarRowKind::Error(ErrorRow {
                role: "error",
                presentation,
                at_ms,
                seq,
                branch_id,
                ordinal: 0,
            })),
            unresolved_tool: None,
        }),
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
    pipe_body_line_with(&mut TranscriptJoiner::default(), envelope)
}

/// Stateful form of [`pipe_body_line`] with the same joined previews as the
/// structured sidecar row.
#[must_use]
pub fn pipe_body_line_with(
    joiner: &mut TranscriptJoiner,
    envelope: &RawEnvelope,
) -> Option<String> {
    sidecar_row_with(joiner, envelope).map(|row| row.pipe_body_line())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::envelope::{EventEnvelope, PromptRender, RenderTargets};
    use crate::error::{ErrorAction, ErrorCode, ErrorPresentation, ErrorScope};
    use crate::history::TreeNode;
    use crate::ids::{BranchId, DeviceId, EventId, ItemId, NodeId, SessionId};
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

    /// MUTATION CHECK (v0.0.935 #3): peek a wrong tag name, skip the peek's
    /// full-decode fallback, or treat the tag alone as a parsed result.
    /// Expected RUNTIME failure: the genuine result below stops matching, or
    /// the tagged-but-malformed payload starts matching.
    #[test]
    fn type_tag_peek_is_exactly_the_full_decode_relevance() {
        let result = BoundedResult {
            preview: "ok".into(),
            truncated: false,
            artifact: None,
            images: Vec::new(),
            cursor: None,
            status: Default::default(),
            reason: None,
            presentation: None,
        };
        let genuine = envelope(
            7,
            EventPayload::ToolResult {
                call_id: "call-peek".into(),
                result: result.clone(),
            },
        );
        let joined = matching_result(&genuine).expect("genuine tool_result matches");
        assert_eq!(joined.0.2, "call-peek");
        assert_eq!(joined.1, result);

        let mut tagged_malformed = envelope(8, EventPayload::IdleDecayed);
        tagged_malformed.payload = serde_json::json!({
            "type": "tool_result",
            "call_id": 7,
        });
        assert!(matching_result(&tagged_malformed).is_none());

        let mut untagged = envelope(9, EventPayload::IdleDecayed);
        untagged.payload = serde_json::json!({"call_id": "call-peek"});
        assert!(matching_result(&untagged).is_none());

        let foreign = envelope(10, EventPayload::IdleDecayed);
        assert!(matching_result(&foreign).is_none());
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

    #[test]
    fn sidecar_rows_pin_branch_and_ordinal_identity() {
        let mut event = node(
            7,
            NodeKind::UserTurn {
                text: "branched".into(),
                attachments: Vec::new(),
            },
        );
        event.branch_id = Some(BranchId::new("branch-seven"));
        assert_eq!(
            sidecar_row_line(&event).as_deref(),
            Some(
                "{\"role\":\"user\",\"text\":\"branched\",\"at_ms\":1700000000007,\"seq\":7,\"branch_id\":\"branch-seven\",\"ordinal\":0}"
            )
        );
    }
}
