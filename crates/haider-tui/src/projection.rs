//! Envelope → display projection: the render-independent core of the session
//! view. A [`SessionProjection`] consumes the session's `RawEnvelope` stream
//! in seq order and maintains everything a screen needs — badge state,
//! transcript entries, pinned todos, open menu, context tokens — so widgets
//! stay pure functions of this struct.
//!
//! Laws honored here:
//! - Item lifecycle (`started`/`delta`/`completed`): `Completed` carries the
//!   final item and REPLACES the block (deltas are advisory, replay-safe).
//! - Forward compatibility: unknown payloads are counted, never fatal.
//! - Honesty: seq gaps and orphan deltas are surfaced as counters, not hidden.
//!
//! Badge strings are sim goldens (`BADGE_LABEL` in the `/tui` sim). Protocol
//! states the sim predates (Queued, Verifying, Concluding, Cancelling) use
//! glyphs consistent with the sim's language. The sim's derived
//! `WAITING`-on-live-subagents state arrives with the subagent wave; here
//! `WAITING` renders only from `RunState::Waiting`.

use base64::Engine as _;
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::menu::Menu;
use haider_protocol::provider::Usage;
use haider_protocol::state::{HarnessStatus, ReadinessCheck, RunState, VerifyStep, WaitReason};

/// Command output kept per block for display — the FULL output lives in the
/// store; the transcript shows a bounded tail (bound at the edge, never let
/// display state grow with process output).
pub const OUTPUT_TAIL_MAX: usize = 8 * 1024;

/// One rendered row of the transcript, in arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// A user message (`EventPayload::UserMessage`).
    User { text: String, attachments: usize },
    /// A turn item and its streaming accumulation state.
    Item(ItemBlock),
    /// A display-only UI note (sim `NoteRow`): auto-title, interrupt, and
    /// mid-turn echoes. The ONLY non-envelope entry source.
    Note { text: String },
}

/// A turn item block plus the delta state that `TurnItem` itself cannot hold
/// (arg fragments while a tool call streams, bounded command output).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemBlock {
    pub item_id: ItemId,
    pub item: TurnItem,
    /// True between `Started` and `Completed`.
    pub streaming: bool,
    /// Accumulated `ToolArgs` fragments (display-only; `Completed` carries
    /// the authoritative parsed args).
    pub args_fragments: String,
    /// Tail of decoded `CommandOutput` bytes, capped at [`OUTPUT_TAIL_MAX`].
    pub output_tail: Vec<u8>,
    /// True once the front of the output was dropped by the cap.
    pub output_truncated: bool,
    /// True if any output chunk failed base64 decoding (chunk skipped).
    pub output_decode_error: bool,
}

impl ItemBlock {
    fn new(item_id: ItemId, item: TurnItem, streaming: bool) -> Self {
        Self {
            item_id,
            item,
            streaming,
            args_fragments: String::new(),
            output_tail: Vec::new(),
            output_truncated: false,
            output_decode_error: false,
        }
    }

    /// The bounded output tail as text (command output is bytes by law;
    /// display is lossy UTF-8). Zero-copy when the tail is valid UTF-8
    /// (efficiency rider #6 — this runs per command block per dirty frame).
    #[must_use]
    pub fn output_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.output_tail)
    }
}

/// The live todo plan pinned above the composer; unpins into the transcript
/// when every item completes (sim behavior).
#[derive(Debug, Clone, PartialEq)]
pub struct TodoPanel {
    pub item_id: ItemId,
    pub items: Vec<TodoItem>,
    pub pinned: bool,
}

impl TodoPanel {
    #[must_use]
    pub fn done_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.state == TodoState::Completed)
            .count()
    }

    /// The item currently marked processing, if any (collapsed-header line).
    #[must_use]
    pub fn current(&self) -> Option<&TodoItem> {
        self.items
            .iter()
            .find(|item| item.state == TodoState::Processing)
    }
}

/// Session display state, reduced from the envelope stream.
#[derive(Debug, Default)]
pub struct SessionProjection {
    last_seq: Option<u64>,
    harness: Option<HarnessStatus>,
    run: Option<RunState>,
    interrupted: bool,
    entries: Vec<TranscriptEntry>,
    /// Item ids whose lifecycle has closed — a re-delivered `Completed` (or a
    /// stale `Started`) for one of these is idempotently ignored (replace
    /// semantics: one item, one block, ever).
    finished_items: std::collections::HashSet<ItemId>,
    menu: Option<Menu>,
    todos: Option<TodoPanel>,
    usage: Option<Usage>,
    // Honesty counters — surfaced, never fatal.
    gap_seen: bool,
    orphan_deltas: u64,
    unknown_payloads: u64,
    duplicate_items: u64,
}

impl SessionProjection {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one raw envelope in stream order. Duplicate seqs are skipped;
    /// gaps are recorded (attach/replay honesty) and processing continues.
    /// Unknown payloads are counted and ignored (forward-compat law).
    /// Envelopes marked `render.ui == false` advance sequence accounting but
    /// never mutate display state (§6.1: three surfaces, never conflated).
    pub fn apply_raw(&mut self, envelope: &RawEnvelope) {
        if let Some(last) = self.last_seq {
            if envelope.seq <= last {
                return;
            }
            if envelope.seq != last + 1 {
                self.gap_seen = true;
            }
        }
        self.last_seq = Some(envelope.seq);
        if !envelope.render.ui {
            return;
        }
        match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
            Ok(payload) => self.apply(&payload),
            Err(_) => self.unknown_payloads += 1,
        }
    }

    /// Consume one typed payload (used by tests and the mock client).
    pub fn apply(&mut self, payload: &EventPayload) {
        match payload {
            EventPayload::HarnessStatus(status) => self.harness = Some(status.clone()),
            EventPayload::SessionState(state) => {
                if let haider_protocol::state::SessionState::Idle { interrupted } = state {
                    self.interrupted = *interrupted;
                }
            }
            EventPayload::RunState(run) => {
                if matches!(run, RunState::Cancelled) {
                    self.interrupted = true;
                } else if !run.is_terminal() {
                    // A new or progressing turn clears the idle(i) marker.
                    self.interrupted = false;
                }
                self.run = Some(run.clone());
            }
            EventPayload::IdleDecayed => self.interrupted = false,
            EventPayload::MenuOpened(menu) => self.menu = Some(menu.clone()),
            EventPayload::MenuAnswered(answer) => {
                if self.menu.as_ref().is_some_and(|m| m.id == answer.menu) {
                    self.menu = None;
                }
            }
            EventPayload::UserMessage {
                text, attachments, ..
            } => self.entries.push(TranscriptEntry::User {
                text: text.clone(),
                attachments: attachments.len(),
            }),
            EventPayload::Item(event) => self.apply_item(event),
            EventPayload::Usage(usage) => self.usage = Some(usage.clone()),
            // Consumed by later waves (effects timeline, subagent tree, gate
            // panel, accounts). The projection stays tolerant of them now.
            EventPayload::Effect(_)
            | EventPayload::ToolResult { .. }
            | EventPayload::NodeCommitted(_)
            | EventPayload::AgentSpawned(_)
            | EventPayload::AgentReport(_)
            | EventPayload::AgentChipState { .. }
            | EventPayload::GateReport(_)
            | EventPayload::Rotation(_) => {}
        }
    }

    fn apply_item(&mut self, event: &ItemEvent) {
        match event {
            ItemEvent::Started { item_id, item } => {
                // Idempotency: a closed id never restarts, an open id never
                // doubles (replay/re-delivery under fresh seqs). Active plans
                // live in `todos`, not `entries` — a stale plan Started must
                // not overwrite a progressed plan (review r2 P2).
                let plan_active = self
                    .todos
                    .as_ref()
                    .is_some_and(|panel| panel.pinned && panel.item_id == *item_id);
                if self.finished_items.contains(item_id)
                    || plan_active
                    || self.open_block_mut(item_id).is_some()
                {
                    self.duplicate_items += 1;
                    return;
                }
                if let TurnItem::Plan { items } = item {
                    self.todos = Some(TodoPanel {
                        item_id: item_id.clone(),
                        items: items.clone(),
                        pinned: true,
                    });
                } else {
                    self.entries.push(TranscriptEntry::Item(ItemBlock::new(
                        item_id.clone(),
                        item.clone(),
                        true,
                    )));
                }
            }
            ItemEvent::Delta { item_id, delta } => self.apply_delta(item_id, delta),
            ItemEvent::Completed { item_id, item } => {
                if self.finished_items.contains(item_id) {
                    self.duplicate_items += 1;
                    return;
                }
                if let TurnItem::Plan { items } = item {
                    let all_done = items.iter().all(|todo| todo.state == TodoState::Completed);
                    self.todos = Some(TodoPanel {
                        item_id: item_id.clone(),
                        items: items.clone(),
                        pinned: !all_done,
                    });
                    if all_done {
                        // The completed plan unpins INTO the transcript and
                        // the id closes — later duplicates are no-ops.
                        self.finished_items.insert(item_id.clone());
                        self.entries.push(TranscriptEntry::Item(ItemBlock::new(
                            item_id.clone(),
                            item.clone(),
                            false,
                        )));
                    }
                } else {
                    self.finished_items.insert(item_id.clone());
                    if let Some(block) = self.open_block_mut(item_id) {
                        // Replace semantics: the final item is authoritative.
                        block.item = item.clone();
                        block.streaming = false;
                        // The completed item carries the parsed args; the raw
                        // fragment accumulation is a duplicate — release it
                        // (efficiency rider #3).
                        block.args_fragments = String::new();
                    } else {
                        // Attach-mid-stream tolerance: a Completed we never
                        // saw start still lands as a finished block.
                        self.entries.push(TranscriptEntry::Item(ItemBlock::new(
                            item_id.clone(),
                            item.clone(),
                            false,
                        )));
                    }
                }
            }
        }
    }

    fn apply_delta(&mut self, item_id: &ItemId, delta: &ItemDelta) {
        let Some(block) = self.open_block_mut(item_id) else {
            self.orphan_deltas += 1;
            return;
        };
        match delta {
            ItemDelta::Text { text } => {
                if let TurnItem::AgentMessage { text: body } = &mut block.item {
                    body.push_str(text);
                }
            }
            ItemDelta::Reasoning { text } => {
                if let TurnItem::Reasoning { summary } = &mut block.item {
                    summary.push_str(text);
                }
            }
            ItemDelta::ToolArgs { fragment } => block.args_fragments.push_str(fragment),
            ItemDelta::CommandOutput { chunk_b64, .. } => {
                match base64::engine::general_purpose::STANDARD.decode(chunk_b64) {
                    Ok(bytes) => {
                        // Bound BEFORE appending so the tail's capacity never
                        // grows past the cap (efficiency rider #4: append-
                        // then-drain retained chunk-sized high-water marks).
                        let keep = bytes.len().min(OUTPUT_TAIL_MAX);
                        let incoming = &bytes[bytes.len() - keep..];
                        if bytes.len() > keep {
                            block.output_truncated = true;
                        }
                        let overflow = (block.output_tail.len() + incoming.len())
                            .saturating_sub(OUTPUT_TAIL_MAX);
                        if overflow > 0 {
                            block.output_tail.drain(..overflow);
                            block.output_truncated = true;
                        }
                        block.output_tail.extend_from_slice(incoming);
                    }
                    Err(_) => block.output_decode_error = true,
                }
            }
        }
    }

    /// The most recent still-streaming block for `item_id` (deltas always
    /// target the open block; searching from the back keeps this O(open)).
    fn open_block_mut(&mut self, item_id: &ItemId) -> Option<&mut ItemBlock> {
        self.entries.iter_mut().rev().find_map(|entry| match entry {
            TranscriptEntry::Item(block) if block.streaming && &block.item_id == item_id => {
                Some(block)
            }
            _ => None,
        })
    }

    /// True while the run is in its THINKING beat — the sim shows a
    /// transient `● thinking…` line at the transcript tail.
    #[must_use]
    pub const fn is_thinking(&self) -> bool {
        matches!(self.run, Some(RunState::Thinking))
    }

    /// True while the idle(i) marker is set (esc interrupted the last turn).
    #[must_use]
    pub const fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Append a display-only note row (sim `NoteRow`): auto-title,
    /// interrupt, mid-turn input echoes. Never sourced from envelopes.
    pub fn push_note(&mut self, text: String) {
        self.entries.push(TranscriptEntry::Note { text });
    }

    /// The status-bar badge, sim `BADGE_LABEL` goldens.
    #[must_use]
    pub fn badge(&self) -> String {
        if matches!(self.harness, Some(HarnessStatus::Starting { .. })) {
            return "◌ STARTING".to_owned();
        }
        let Some(run) = &self.run else {
            return self.idle_label();
        };
        match run {
            RunState::Queued => "◌ QUEUED".to_owned(),
            RunState::Thinking => "● THINKING".to_owned(),
            RunState::Streaming => "▮ STREAMING".to_owned(),
            RunState::RunningTool => "⚒ TOOL_RUNNING".to_owned(),
            RunState::Waiting { reason } => format!("◔ WAITING · {}", wait_reason_label(reason)),
            RunState::InputRequired { .. } => "? INPUT_REQUIRED".to_owned(),
            RunState::PermissionRequired { .. } => "? PERMISSION_REQUIRED".to_owned(),
            RunState::Compacting => "⊟ COMPACTING".to_owned(),
            RunState::Verifying { step } => {
                format!("⚙ VERIFYING · {}", verify_step_label(*step))
            }
            RunState::Concluding => "◆ CONCLUDING".to_owned(),
            RunState::EffectOutcomeUnknown => "⌁ EFFECT_UNKNOWN".to_owned(),
            RunState::Cancelling => "⊘ CANCELLING".to_owned(),
            RunState::Errored => "✗ ERRORED".to_owned(),
            RunState::Done | RunState::Cancelled => self.idle_label(),
        }
    }

    fn idle_label(&self) -> String {
        if self.interrupted {
            "⏸ IDLE (i)".to_owned()
        } else {
            "IDLE".to_owned()
        }
    }

    /// Visual class of the badge — the sim's `BADGE_OUTLINE`/`badgeTone`
    /// vocabulary (tui.js:5531-5547): idle AND interrupted-idle both fall
    /// through to the QUIET dim outline — the `⏸ IDLE (i)` label carries
    /// the distinction (review r2 P2-11); waiting/starting outline gold;
    /// needs-you states outline warn (never filled); effect-unknown
    /// outlines err; fills are reserved for live machinery (gold work ·
    /// maroon tool · warn compaction · err failure).
    #[must_use]
    pub fn badge_tone(&self) -> BadgeTone {
        if matches!(self.harness, Some(HarnessStatus::Starting { .. })) {
            return BadgeTone::Restful;
        }
        match &self.run {
            None | Some(RunState::Done | RunState::Cancelled) => BadgeTone::Idle,
            Some(RunState::Queued | RunState::Waiting { .. }) => BadgeTone::Restful,
            Some(
                RunState::Thinking
                | RunState::Streaming
                | RunState::Concluding
                | RunState::Verifying { .. },
            ) => BadgeTone::Active,
            Some(RunState::RunningTool | RunState::Cancelling) => BadgeTone::Tool,
            Some(RunState::Compacting) => BadgeTone::Compacting,
            Some(RunState::InputRequired { .. } | RunState::PermissionRequired { .. }) => {
                BadgeTone::Attention
            }
            Some(RunState::EffectOutcomeUnknown) => BadgeTone::EffectUnknown,
            Some(RunState::Errored) => BadgeTone::Error,
        }
    }

    /// Boot-screen readiness checklist while the harness is starting.
    #[must_use]
    pub fn boot_checks(&self) -> Option<&[ReadinessCheck]> {
        match &self.harness {
            Some(HarnessStatus::Starting { checks }) => Some(checks),
            _ => None,
        }
    }

    /// Context-meter tokens: the latest usage frame's total footprint
    /// (input + cached + output + reasoning). v0 upper-bound approximation —
    /// exact per-provider window accounting lands with the real adapters.
    /// Saturating: adversarial usage frames must not panic the meter.
    #[must_use]
    pub fn context_tokens(&self) -> u64 {
        self.usage.as_ref().map_or(0, |u| {
            u.input
                .saturating_add(u.cached)
                .saturating_add(u.output)
                .saturating_add(u.reasoning)
        })
    }

    #[must_use]
    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    #[must_use]
    pub fn open_menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    #[must_use]
    pub fn todos(&self) -> Option<&TodoPanel> {
        self.todos.as_ref()
    }

    #[must_use]
    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    #[must_use]
    pub fn gap_seen(&self) -> bool {
        self.gap_seen
    }

    #[must_use]
    pub fn orphan_deltas(&self) -> u64 {
        self.orphan_deltas
    }

    #[must_use]
    pub fn unknown_payloads(&self) -> u64 {
        self.unknown_payloads
    }

    #[must_use]
    pub fn duplicate_items(&self) -> u64 {
        self.duplicate_items
    }
}

/// Badge visual class — see [`SessionProjection::badge_tone`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadgeTone {
    /// Plain IDLE — quiet dim outline (sim: dim ink, frame border).
    Idle,
    /// Notable-but-restful outline states — interrupted idle, waiting,
    /// starting, queued: gold outline.
    Restful,
    /// Active thinking/streaming work: gold fill.
    Active,
    /// Tool machinery: maroon fill.
    Tool,
    /// Context compaction: warn fill.
    Compacting,
    /// Needs-you (permission / input required): warn OUTLINE, never filled.
    Attention,
    /// Effect outcome unknown: err outline.
    EffectUnknown,
    /// Failure: err fill.
    Error,
}

fn wait_reason_label(reason: &WaitReason) -> String {
    match reason {
        WaitReason::ProviderBackoff => "provider backoff".to_owned(),
        WaitReason::RateLimit => "rate limit".to_owned(),
        WaitReason::RemoteChild => "remote subagent".to_owned(),
        WaitReason::LocalChild => "subagent".to_owned(),
        WaitReason::DeviceUnreachable => "device unreachable".to_owned(),
        WaitReason::BlockingHook => "hook".to_owned(),
        WaitReason::Dependency => "dependency".to_owned(),
        WaitReason::VerifyHold => "verify hold".to_owned(),
        WaitReason::VerifyQueue => "verify queue".to_owned(),
        WaitReason::WorkspaceSettlement => "workspace settlement".to_owned(),
        WaitReason::WorkspaceVerify => "workspace verify".to_owned(),
        WaitReason::Other { tag } => tag.clone(),
    }
}

fn verify_step_label(step: VerifyStep) -> &'static str {
    match step {
        VerifyStep::Check => "check",
        VerifyStep::Format => "format",
        VerifyStep::Test => "test",
    }
}
