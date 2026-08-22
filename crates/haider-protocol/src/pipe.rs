//! Shared transcript projections for JSONL sidecars and instruct-pipe exports.

use crate::EventPayload;
use crate::envelope::RawEnvelope;
use crate::history::NodeKind;
use crate::item::{ItemDelta, ItemEvent, TurnItem};
use crate::state::RunState;
use crate::tool::BoundedResult;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

/// Maximum number of Unicode scalar values carried by a cold-history tool
/// argument or result preview.
pub const TOOL_PREVIEW_CHARS: usize = 160;
const MAX_PENDING_TOOL_RESULTS: usize = 1_024;
type ToolJoinKey = (Option<String>, Option<String>, String);
type ProjectionRunKey = (Option<String>, Option<String>, String);

#[derive(Debug)]
struct OpenReasoning {
    item_id: String,
    first_seq: u64,
}

#[derive(Debug)]
struct PendingReasoning {
    summary: String,
    seq: u64,
}

#[derive(Debug)]
struct PendingCompaction {
    first_seq: u64,
}

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
    open_reasoning: HashMap<ProjectionRunKey, OpenReasoning>,
    pending_reasoning: HashMap<ProjectionRunKey, VecDeque<PendingReasoning>>,
    compacting_runs: HashMap<ProjectionRunKey, PendingCompaction>,
    /// Runs that committed item events. An `assistant_commit` node in such a
    /// run repeats content the item stream already carries, so its text row
    /// is marked `compat` and an item-canonical client may drop it
    /// unconditionally. Pre-item journals never populate this, so their rows
    /// stay unmarked and fold normally.
    item_runs: std::collections::HashSet<String>,
}

struct BufferedRow {
    row: SidecarRow,
    unresolved_tool: Option<ToolJoinKey>,
    unresolved_reasoning: Option<ProjectionRunKey>,
    remaining_facts: usize,
}

enum ReasoningFact {
    Started { item_id: String },
    Delta { item_id: String },
    Sealed { item_id: String, summary: String },
}

impl TranscriptProjector {
    /// Rebuild join state through a cursor without projecting rows that are
    /// already durable. Rows after that cursor still enter through [`Self::push`]
    /// and may wait for a later result as usual.
    pub fn prewarm(&mut self, envelope: &RawEnvelope) {
        self.note_item_run(envelope);
        let _ = self.joiner.observe(envelope);
    }

    /// Cheap type-tag peek: remembers which runs speak item events, without
    /// decoding the payload.
    fn note_item_run(&mut self, envelope: &RawEnvelope) {
        if envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            == Some("item")
            && let Some(run_id) = envelope.run_id.as_ref()
        {
            self.item_runs.insert(run_id.as_str().to_owned());
        }
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

        self.note_item_run(envelope);
        let run_key = projection_run_key(envelope);
        if let (Some(run_key), Some(fact)) = (run_key.as_ref(), reasoning_fact(envelope)) {
            self.observe_reasoning(run_key, envelope.seq, fact);
        }
        if is_compaction_node(envelope)
            && let Some(run_key) = run_key.as_ref()
        {
            self.compacting_runs
                .entry(run_key.clone())
                .or_insert(PendingCompaction {
                    first_seq: envelope.seq,
                });
        }
        let run_repeats_items = envelope
            .run_id
            .as_ref()
            .is_some_and(|run_id| self.item_runs.contains(run_id.as_str()));
        let result = matching_result(envelope);
        if let Some(projection) = sidecar_projection(&mut self.joiner, envelope, run_repeats_items)
        {
            let mut row = projection.row;
            // A projected row with NO run scope is a turn boundary — a user
            // turn. It ends every open turn at once, so whatever was being
            // thought before the user spoke cannot belong to an answer that
            // comes after them. The run-scoped cleanup below cannot do this:
            // it is keyed on the INCOMING row's run, and a user turn has none,
            // so without this the orphaned summary survives the boundary and
            // attaches to the next assistant row of the earlier run.
            if run_key.is_none() && !row.is_attachable_assistant() {
                self.pending_reasoning.clear();
            }
            let unresolved_reasoning = run_key.as_ref().and_then(|run_key| {
                if !row.is_attachable_assistant() {
                    self.settle_waiting_assistants(run_key);
                    self.pending_reasoning.remove(run_key);
                    return None;
                }
                self.settle_waiting_assistants(run_key);
                if let Some(pending) = self
                    .pending_reasoning
                    .get_mut(run_key)
                    .and_then(VecDeque::pop_front)
                {
                    row.set_reasoning_summary(pending.summary);
                    if self
                        .pending_reasoning
                        .get(run_key)
                        .is_some_and(VecDeque::is_empty)
                    {
                        self.pending_reasoning.remove(run_key);
                    }
                    None
                } else {
                    self.open_reasoning
                        .contains_key(run_key)
                        .then(|| run_key.clone())
                }
            });
            self.buffered.push_back(BufferedRow {
                row,
                unresolved_tool: projection.unresolved_tool,
                unresolved_reasoning,
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

        if is_terminal_run_state(envelope)
            && let Some(run_key) = run_key.as_ref()
        {
            self.settle_waiting_assistants(run_key);
            self.open_reasoning.remove(run_key);
            self.pending_reasoning.remove(run_key);
            if self.compacting_runs.remove(run_key).is_some() {
                self.buffered.push_back(BufferedRow {
                    row: SidecarRow(SidecarRowKind::CompactionBoundary(CompactionBoundaryRow {
                        kind: "compaction_boundary",
                        at_ms: envelope.committed_at_ms,
                        seq: envelope.seq,
                        branch_id: envelope
                            .branch_id
                            .as_ref()
                            .map(|branch| branch.as_str().to_owned()),
                        run_id: run_key.2.clone(),
                        ordinal: 0,
                    })),
                    unresolved_tool: None,
                    unresolved_reasoning: None,
                    remaining_facts: MAX_PENDING_TOOL_RESULTS,
                });
            }
        }

        self.take_ready_rows()
    }

    /// Flush final unresolved rows without fabricating results.
    pub fn finish(&mut self) -> Vec<SidecarRow> {
        self.open_reasoning.clear();
        self.pending_reasoning.clear();
        self.compacting_runs.clear();
        self.buffered
            .drain(..)
            .map(|buffered| buffered.row)
            .collect()
    }

    /// Flush unresolved tool joins at a durable journal EOF while preserving
    /// turn state whose later seal or terminal event changes the projection.
    pub fn flush_unresolved_tools(&mut self) -> Vec<SidecarRow> {
        for buffered in &mut self.buffered {
            buffered.unresolved_tool = None;
        }
        self.take_ready_rows()
    }

    /// Earliest row withheld from durable sidecar coverage.
    #[must_use]
    pub fn blocked_seq(&self) -> Option<u64> {
        self.buffered
            .front()
            .map(|buffered| buffered.row.seq())
            .into_iter()
            .chain(self.earliest_lifecycle_blocked_seq())
            .min()
    }

    fn observe_reasoning(&mut self, run_key: &ProjectionRunKey, seq: u64, fact: ReasoningFact) {
        match fact {
            ReasoningFact::Started { item_id } | ReasoningFact::Delta { item_id } => {
                self.open_reasoning
                    .entry(run_key.clone())
                    .or_insert(OpenReasoning {
                        item_id,
                        first_seq: seq,
                    });
            }
            ReasoningFact::Sealed { item_id, summary } => {
                let matching_open = self
                    .open_reasoning
                    .get(run_key)
                    .is_some_and(|open| open.item_id == item_id);
                if self.open_reasoning.contains_key(run_key) && !matching_open {
                    return;
                }
                if matching_open {
                    self.open_reasoning.remove(run_key);
                }
                if let Some(buffered) = self
                    .buffered
                    .iter_mut()
                    .rev()
                    .find(|buffered| buffered.unresolved_reasoning.as_ref() == Some(run_key))
                {
                    buffered.row.set_reasoning_summary(summary);
                    buffered.unresolved_reasoning = None;
                } else {
                    self.pending_reasoning
                        .entry(run_key.clone())
                        .or_default()
                        .push_back(PendingReasoning { summary, seq });
                }
            }
        }
    }

    fn settle_waiting_assistants(&mut self, run_key: &ProjectionRunKey) {
        for buffered in &mut self.buffered {
            if buffered.unresolved_reasoning.as_ref() == Some(run_key) {
                buffered.unresolved_reasoning = None;
            }
        }
    }

    fn take_ready_rows(&mut self) -> Vec<SidecarRow> {
        let mut rows = Vec::new();
        let lifecycle_barrier = self.earliest_lifecycle_blocked_seq();
        while self.buffered.front().is_some_and(|buffered| {
            buffered.unresolved_tool.is_none()
                && buffered.unresolved_reasoning.is_none()
                && lifecycle_barrier.is_none_or(|barrier| buffered.row.seq() < barrier)
        }) {
            if let Some(buffered) = self.buffered.pop_front() {
                rows.push(buffered.row);
            }
        }
        rows
    }

    /// An unresolved turn artifact is a global ordering barrier, not merely a
    /// coverage constraint. Emitting a later ready row would either duplicate
    /// it when replay starts before the barrier, or make a max-seq reader skip
    /// the still-unsealed artifact forever.
    fn earliest_lifecycle_blocked_seq(&self) -> Option<u64> {
        self.open_reasoning
            .values()
            .map(|reasoning| reasoning.first_seq)
            .chain(
                self.pending_reasoning
                    .values()
                    .filter_map(|pending| pending.front().map(|reasoning| reasoning.seq)),
            )
            .chain(
                self.compacting_runs
                    .values()
                    .map(|compaction| compaction.first_seq),
            )
            .min()
    }
}

fn projection_run_key(envelope: &RawEnvelope) -> Option<ProjectionRunKey> {
    Some((
        envelope
            .branch_id
            .as_ref()
            .map(|branch| branch.as_str().to_owned()),
        envelope
            .agent_id
            .as_ref()
            .map(|agent| agent.as_str().to_owned()),
        envelope.run_id.as_ref()?.as_str().to_owned(),
    ))
}

fn reasoning_fact(envelope: &RawEnvelope) -> Option<ReasoningFact> {
    let payload = &envelope.payload;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("item") {
        return None;
    }
    let event = payload.get("event").and_then(serde_json::Value::as_str);
    let exact_reasoning_shape = match event {
        Some("started" | "completed") => {
            payload
                .get("item")
                .and_then(|item| item.get("item"))
                .and_then(serde_json::Value::as_str)
                == Some("reasoning")
        }
        Some("delta") => {
            payload
                .get("delta")
                .and_then(|delta| delta.get("delta"))
                .and_then(serde_json::Value::as_str)
                == Some("reasoning")
        }
        _ => false,
    };
    if !exact_reasoning_shape {
        return None;
    }
    match serde_json::from_value::<EventPayload>(payload.clone()).ok()? {
        EventPayload::Item(ItemEvent::Started {
            item_id,
            item: TurnItem::Reasoning { .. },
        }) => Some(ReasoningFact::Started {
            item_id: item_id.as_str().to_owned(),
        }),
        EventPayload::Item(ItemEvent::Delta {
            item_id,
            delta: ItemDelta::Reasoning { .. },
        }) => Some(ReasoningFact::Delta {
            item_id: item_id.as_str().to_owned(),
        }),
        EventPayload::Item(ItemEvent::Completed {
            item_id,
            item: TurnItem::Reasoning { summary },
        }) => Some(ReasoningFact::Sealed {
            item_id: item_id.as_str().to_owned(),
            summary,
        }),
        _ => None,
    }
}

fn is_compaction_node(envelope: &RawEnvelope) -> bool {
    let payload = &envelope.payload;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("node_committed")
        || payload
            .get("kind")
            .and_then(|kind| kind.get("kind"))
            .and_then(serde_json::Value::as_str)
            != Some("compaction")
    {
        return false;
    }
    matches!(
        serde_json::from_value::<EventPayload>(payload.clone()),
        Ok(EventPayload::NodeCommitted(crate::history::TreeNode {
            kind: NodeKind::Compaction { .. },
            ..
        }))
    )
}

fn is_terminal_run_state(envelope: &RawEnvelope) -> bool {
    let payload = &envelope.payload;
    if payload.get("type").and_then(serde_json::Value::as_str) != Some("run_state") {
        return false;
    }
    matches!(
        serde_json::from_value::<EventPayload>(payload.clone()),
        Ok(EventPayload::RunState(
            RunState::Done | RunState::Errored | RunState::Cancelled
        ))
    )
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
    /// Final `TurnItem::Reasoning.summary` for this assistant response. Pipe
    /// projection never serializes streaming reasoning deltas.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    ordinal: u32,
    /// Marks a row whose content the item stream ALSO carries (or an empty
    /// turn-start row, which is always safe to drop). Serialized only when
    /// true, so pre-item journals and exports keep their exact bytes.
    #[serde(skip_serializing_if = "is_false")]
    compat: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
struct IncompleteRow {
    role: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
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

#[derive(Serialize)]
struct CompactionBoundaryRow {
    kind: &'static str,
    at_ms: u64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_id: Option<String>,
    run_id: String,
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
    CompactionBoundary(CompactionBoundaryRow),
}

impl SidecarRow {
    fn set_result_preview(&mut self, preview: Option<String>) {
        if let SidecarRowKind::Tool(row) = &mut self.0 {
            row.result_preview = preview;
        }
    }

    fn is_attachable_assistant(&self) -> bool {
        matches!(
            &self.0,
            SidecarRowKind::Text(TextRow {
                role: "assistant",
                ..
            }) | SidecarRowKind::Incomplete(_)
        )
    }

    fn set_reasoning_summary(&mut self, summary: String) {
        let summary = (!summary.is_empty()).then_some(summary);
        match &mut self.0 {
            SidecarRowKind::Text(row) if row.role == "assistant" => row.reasoning = summary,
            SidecarRowKind::Incomplete(row) => row.reasoning = summary,
            _ => {}
        }
    }

    /// Whether this row closes a compacting turn and therefore seals the
    /// current physical JSONL segment.
    #[must_use]
    pub fn is_compaction_boundary(&self) -> bool {
        matches!(&self.0, SidecarRowKind::CompactionBoundary(_))
    }

    /// Journal sequence that produced this transcript row.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match &self.0 {
            SidecarRowKind::Text(row) => row.seq,
            SidecarRowKind::Incomplete(row) => row.seq,
            SidecarRowKind::Error(row) => row.seq,
            SidecarRowKind::Tool(row) => row.seq,
            SidecarRowKind::CompactionBoundary(row) => row.seq,
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
            SidecarRowKind::CompactionBoundary(row) => {
                format!("C  {} {} |compaction boundary|", row.seq, row.at_ms)
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
    // The stateless form has no run history, so only the always-safe empty
    // assistant rows are marked here; the pipe projector carries the run
    // tracking that marks item-repeating content.
    sidecar_projection(joiner, envelope, false).map(|projection| projection.row)
}

fn sidecar_projection(
    joiner: &mut TranscriptJoiner,
    envelope: &RawEnvelope,
    run_repeats_items: bool,
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
                    reasoning: None,
                    at_ms,
                    seq,
                    branch_id,
                    ordinal: 0,
                    compat: false,
                })),
                unresolved_tool: None,
            }),
            NodeKind::AssistantCommit { text, .. } => Some(SidecarProjection {
                row: SidecarRow(SidecarRowKind::Text(TextRow {
                    // Empty turn-start commits precede their run's items, so
                    // they are marked unconditionally: dropping an empty row
                    // loses nothing on any journal.
                    compat: run_repeats_items || text.is_empty(),
                    role: "assistant",
                    text,
                    reasoning: None,
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
                reasoning: None,
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
    use crate::history::CompactionResume;
    use crate::history::TreeNode;
    use crate::ids::{ArtifactRef, BranchId, DeviceId, EventId, ItemId, NodeId, RunId, SessionId};
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

    fn in_run(mut envelope: RawEnvelope, run_id: &str) -> RawEnvelope {
        envelope.run_id = Some(RunId::new(run_id));
        envelope
    }

    fn reasoning_started(seq: u64, run_id: &str, item_id: &str) -> RawEnvelope {
        in_run(
            envelope(
                seq,
                EventPayload::Item(ItemEvent::Started {
                    item_id: ItemId::new(item_id),
                    item: TurnItem::Reasoning {
                        summary: String::new(),
                    },
                }),
            ),
            run_id,
        )
    }

    fn reasoning_delta(seq: u64, run_id: &str, item_id: &str, text: &str) -> RawEnvelope {
        in_run(
            envelope(
                seq,
                EventPayload::Item(ItemEvent::Delta {
                    item_id: ItemId::new(item_id),
                    delta: ItemDelta::Reasoning {
                        text: text.to_owned(),
                    },
                }),
            ),
            run_id,
        )
    }

    fn reasoning_sealed(seq: u64, run_id: &str, item_id: &str, summary: &str) -> RawEnvelope {
        in_run(
            envelope(
                seq,
                EventPayload::Item(ItemEvent::Completed {
                    item_id: ItemId::new(item_id),
                    item: TurnItem::Reasoning {
                        summary: summary.to_owned(),
                    },
                }),
            ),
            run_id,
        )
    }

    fn assistant(seq: u64, run_id: &str, text: &str) -> RawEnvelope {
        in_run(
            node(
                seq,
                NodeKind::AssistantCommit {
                    text: text.to_owned(),
                    verdict: VerifyVerdict::Unverified,
                },
            ),
            run_id,
        )
    }

    fn run_state(seq: u64, run_id: &str, state: RunState) -> RawEnvelope {
        in_run(envelope(seq, EventPayload::RunState(state)), run_id)
    }

    fn compaction(seq: u64, run_id: &str, suffix: &str) -> RawEnvelope {
        in_run(
            node(
                seq,
                NodeKind::Compaction {
                    covers_from: NodeId::new(format!("from-{suffix}")),
                    covers_to: NodeId::new(format!("to-{suffix}")),
                    summary_artifact: ArtifactRef::new(format!("artifact-{suffix}")),
                    tokens_before: 100,
                    tokens_after: 10,
                    resume_cause: CompactionResume::AutoMidTurn,
                },
            ),
            run_id,
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

    /// MUTATION CHECK: change the completed-item discriminator path from
    /// `payload.item.item` to `payload.delta.delta`. Expected RUNTIME failure:
    /// the assistant row below loses its sealed `reasoning` summary.
    #[test]
    fn sealed_reasoning_reaches_the_assistant_row() {
        let mut projector = TranscriptProjector::default();
        assert!(
            projector
                .push(&reasoning_started(1, "run-a", "reason-a"))
                .is_empty()
        );
        assert!(
            projector
                .push(&reasoning_delta(2, "run-a", "reason-a", "stream fragment"))
                .is_empty()
        );
        assert!(projector.push(&assistant(3, "run-a", "answer")).is_empty());
        let rows = projector.push(&reasoning_sealed(4, "run-a", "reason-a", "sealed summary"));
        assert_eq!(rows.len(), 1);
        let row = serde_json::to_value(&rows[0]).expect("row serializes");
        assert_eq!(row["text"], "answer");
        assert_eq!(row["reasoning"], "sealed summary");
    }

    /// MUTATION CHECK: assign `ItemDelta::Reasoning.text` to the row's
    /// optional reasoning field. Expected RUNTIME failure: the terminally
    /// released row gains `"reasoning":"stream-only"` instead of omitting it.
    #[test]
    fn reasoning_deltas_are_never_carried() {
        let mut projector = TranscriptProjector::default();
        assert!(
            projector
                .push(&reasoning_started(1, "run-delta", "reason-delta"))
                .is_empty()
        );
        assert!(
            projector
                .push(&reasoning_delta(
                    2,
                    "run-delta",
                    "reason-delta",
                    "stream-only",
                ))
                .is_empty()
        );
        assert!(
            projector
                .push(&assistant(3, "run-delta", "answer"))
                .is_empty()
        );
        let rows = projector.push(&run_state(4, "run-delta", RunState::Done));
        assert_eq!(rows.len(), 1);
        let row = serde_json::to_value(&rows[0]).expect("row serializes");
        assert!(row.get("reasoning").is_none(), "delta leaked: {row}");
    }

    /// MUTATION CHECK: replace the `(branch, agent, run)` lookup with one
    /// global pending assistant. Expected RUNTIME failure: `sealed-a` lands on
    /// run B's row or releases the two rows out of journal order.
    #[test]
    fn sealed_reasoning_attaches_to_the_correct_interleaved_turn() {
        let mut projector = TranscriptProjector::default();
        projector.push(&reasoning_started(1, "run-a", "reason-a"));
        projector.push(&assistant(2, "run-a", "answer-a"));
        projector.push(&reasoning_started(3, "run-b", "reason-b"));
        assert!(
            projector
                .push(&assistant(4, "run-b", "answer-b"))
                .is_empty()
        );

        let first = projector.push(&reasoning_sealed(5, "run-a", "reason-a", "sealed-a"));
        assert_eq!(first.len(), 1);
        let first = serde_json::to_value(&first[0]).expect("first row serializes");
        assert_eq!(first["text"], "answer-a");
        assert_eq!(first["reasoning"], "sealed-a");

        let second = projector.push(&reasoning_sealed(6, "run-b", "reason-b", "sealed-b"));
        assert_eq!(second.len(), 1);
        let second = serde_json::to_value(&second[0]).expect("second row serializes");
        assert_eq!(second["text"], "answer-b");
        assert_eq!(second["reasoning"], "sealed-b");
    }

    /// Reasoning that never found its assistant row must be DISCARDED when a
    /// non-attachable row settles the run, not held for whatever assistant row
    /// comes next. Holding it is how a turn's thinking ends up stapled to a
    /// later answer it did not produce — the same wrong-turn failure as the
    /// cross-run case above, one scope in.
    ///
    /// MUTATION CHECK (executed): delete `self.pending_reasoning.remove(run_key)`
    /// from the non-attachable branch. Expected RUNTIME failure: the assertion
    /// below — the stale summary attaches to the later assistant row. This gap
    /// was found by mutation: the whole protocol and pipe suite passed without
    /// that line, so nothing pinned it.
    #[test]
    fn reasoning_orphaned_by_a_non_assistant_row_never_attaches_later() {
        let mut projector = TranscriptProjector::default();
        projector.push(&reasoning_started(1, "run-a", "reason-a"));
        projector.push(&reasoning_sealed(
            2,
            "run-a",
            "reason-a",
            "orphaned-thinking",
        ));
        // A USER TURN is unambiguously a new turn: whatever the model was
        // thinking before it cannot belong to an answer that comes after it.
        projector.push(&node(
            3,
            NodeKind::UserTurn {
                text: "a new question".into(),
                attachments: Vec::new(),
            },
        ));

        let mut rows = projector.push(&assistant(4, "run-a", "later-answer"));
        // The assistant row may be held pending; `finish` releases the tail.
        rows.extend(projector.finish());
        let later = rows
            .iter()
            .map(|row| serde_json::to_value(row).expect("row serializes"))
            .find(|row| row["text"] == "later-answer")
            .expect("the later assistant row is emitted");
        assert!(
            later.get("reasoning").is_none(),
            "orphaned reasoning must not attach to a later turn: {later}"
        );
    }

    /// MUTATION CHECK: remove the lifecycle barrier from `take_ready_rows`.
    /// Expected RUNTIME failure: the unrelated seq-2 user row escapes before
    /// seq-1 reasoning is sealed, making replay duplicate or skip durable rows.
    #[test]
    fn unresolved_reasoning_is_a_global_row_ordering_barrier() {
        let mut projector = TranscriptProjector::default();
        assert!(
            projector
                .push(&reasoning_started(1, "run-a", "reason-a"))
                .is_empty()
        );
        assert!(
            projector
                .push(&node(
                    2,
                    NodeKind::UserTurn {
                        text: "unrelated".into(),
                        attachments: Vec::new(),
                    },
                ))
                .is_empty()
        );
        let released = projector.push(&reasoning_sealed(3, "run-a", "reason-a", "sealed-a"));
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].seq(), 2);
        let rows = projector.push(&assistant(4, "run-a", "answer-a"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq(), 4);
        assert_eq!(
            serde_json::to_value(&rows[0]).expect("assistant serializes")["reasoning"],
            "sealed-a"
        );
    }

    /// MUTATION CHECK: emit a boundary from `NodeKind::Compaction` instead of
    /// the terminal run-state arm. Expected RUNTIME failure: the intermediate
    /// `Thinking` observation below already returns a boundary row.
    #[test]
    fn compaction_boundary_appears_only_when_the_turn_settles() {
        let mut projector = TranscriptProjector::default();
        assert!(
            projector
                .push(&compaction(1, "compact-run", "one"))
                .is_empty()
        );
        assert!(
            projector
                .push(&run_state(2, "compact-run", RunState::Thinking))
                .is_empty()
        );
        let rows = projector.push(&run_state(3, "compact-run", RunState::Done));
        assert_eq!(rows.len(), 1);
        let row = serde_json::to_value(&rows[0]).expect("boundary serializes");
        assert_eq!(row["kind"], "compaction_boundary");
        assert_eq!(row["seq"], 3);
        assert_eq!(
            rows[0].pipe_body_line(),
            "C  3 1700000000003 |compaction boundary|"
        );
    }

    /// MUTATION CHECK: let a settled compacting run bypass an earlier open
    /// compacting run. Expected RUNTIME failure: run B's boundary is emitted
    /// at seq 3 and seals a segment while run A's seq-1 reset is unresolved.
    #[test]
    fn unresolved_compaction_is_a_global_row_ordering_barrier() {
        let mut projector = TranscriptProjector::default();
        assert!(projector.push(&compaction(1, "run-a", "a")).is_empty());
        assert!(projector.push(&compaction(2, "run-b", "b")).is_empty());
        assert!(
            projector
                .push(&run_state(3, "run-b", RunState::Done))
                .is_empty()
        );
        let rows = projector.push(&run_state(4, "run-a", RunState::Done));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq(), 3);
        assert_eq!(rows[1].seq(), 4);
        assert!(rows.iter().all(SidecarRow::is_compaction_boundary));
    }

    /// MUTATION CHECK: push one boundary row for every observed compaction
    /// node. Expected RUNTIME failure: two passes below produce two rows
    /// instead of exactly one row at the compacting turn's terminal state.
    #[test]
    fn multiple_compaction_passes_in_one_turn_emit_one_boundary() {
        let mut projector = TranscriptProjector::default();
        assert!(
            projector
                .push(&compaction(1, "compact-run", "one"))
                .is_empty()
        );
        assert!(
            projector
                .push(&compaction(2, "compact-run", "two"))
                .is_empty()
        );
        let rows = projector.push(&run_state(3, "compact-run", RunState::Done));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_compaction_boundary());
    }
}
