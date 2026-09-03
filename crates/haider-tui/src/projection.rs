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
use haider_protocol::error::{ErrorAction, ErrorPresentation};
use haider_protocol::history::{TodoItem, TodoState};
use haider_protocol::ids::{EffectId, ItemId, NodeId};
use haider_protocol::item::{ItemDelta, ItemEvent, TurnItem};
use haider_protocol::menu::Menu;
use haider_protocol::peer::{PeerDelivery, PeerKind};
use haider_protocol::provider::Usage;
use haider_protocol::state::{HarnessStatus, ReadinessCheck, RunState, VerifyStep, WaitReason};
use std::fmt::Write as _;

/// Command output kept per block for display — the FULL output lives in the
/// store; the transcript shows a bounded tail (bound at the edge, never let
/// display state grow with process output).
pub const OUTPUT_TAIL_MAX: usize = 8 * 1024;

const fn peer_kind_label(kind: PeerKind) -> &'static str {
    match kind {
        PeerKind::HaiderSession => "haider_session",
        PeerKind::External => "external",
    }
}

/// One rendered row of the transcript, in arrival order.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// A user message (`EventPayload::UserMessage`). `voice` marks a
    /// spoken submission (◉ sigil + ` · spoken` tag) — demo-local: the
    /// protocol has no voice surface yet, so the reducer pushes these.
    /// `from_main` marks a PARENT-AUTHORED row in a chip transcript
    /// (S3: → sigil + ` · from main` tag) — stream truth, never a guess:
    /// the daemon's child-prompt projection is the only writer of
    /// agent-scoped user rows, so every one of them crossed the
    /// parent→child boundary (spawn prompt, tool steer, chip-composer
    /// send alike).
    User {
        text: String,
        attachments: usize,
        voice: bool,
        from_main: bool,
    },
    /// A boundary-delivered message from another agent. This is its own
    /// taxonomy member: it must never inherit the user-row sigil, counting,
    /// history recall, or instruction semantics. Every peer row is untrusted
    /// by construction; callers cannot turn the marker off.
    Peer {
        msg_id: String,
        sender: String,
        sender_kind: String,
        text: String,
        receipt: Option<PeerDelivery>,
    },
    /// A turn item and its streaming accumulation state.
    Item(ItemBlock),
    /// A display-only UI note (sim `NoteRow`): auto-title, interrupt, and
    /// mid-turn echoes. The ONLY non-envelope entry source besides Shell.
    Note { text: String },
    /// A daemon-enforced provider capability refusal. This is deliberately
    /// neither a failed run nor a generic note: the model can adapt and the
    /// turn remains healthy.
    Refusal {
        provider: String,
        tool: String,
        reason: String,
    },
    /// A failed run's public reason. `text` is the plain/greppable
    /// authority; a typed presentation enables the structured card render.
    /// Client-observed and legacy wire failures carry `None`.
    Error {
        text: String,
        presentation: Option<ErrorPresentation>,
    },
    /// A shell builtin run against the demo VFS (sim ShellRow,
    /// tui.js:3910-3918) — deliberately envelope-free: the sim bypasses
    /// the model/harness ("local, instant, no model turn").
    Shell { cmd: String, out: String },
}

/// A turn item block plus the delta state that `TurnItem` itself cannot hold
/// (arg fragments while a tool call streams, bounded command output).
#[derive(Debug, Clone, PartialEq)]
pub struct ItemBlock {
    pub item_id: ItemId,
    pub item: TurnItem,
    /// The hidden durable origin marker linked this command item to a direct
    /// user `!` submission. Model-origin command executions remain false.
    pub user_command: bool,
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
    /// Bounded terminal reason joined from the matching `ToolResult` fact.
    pub tool_reason: Option<String>,
    /// The block was produced during a voice turn — the agent header tags
    /// ` · ♪ speaking` (sim tui.js:3895-3897; demo-local voice surface).
    pub spoken: bool,
    /// Byte starts for logical lines in a large agent message. Built once
    /// while ingesting raw text so viewport layout can seek directly to a
    /// small line window without rescanning the whole message each frame.
    /// Empty for other item kinds and small messages.
    pub(crate) agent_line_starts: Vec<u32>,
}

impl ItemBlock {
    fn new(item_id: ItemId, item: TurnItem, streaming: bool) -> Self {
        let agent_line_starts = index_agent_lines(&item);
        Self {
            item_id,
            item,
            user_command: false,
            streaming,
            args_fragments: String::new(),
            output_tail: Vec::new(),
            output_truncated: false,
            output_decode_error: false,
            tool_reason: None,
            spoken: false,
            agent_line_starts,
        }
    }

    fn new_spoken(item_id: ItemId, item: TurnItem, streaming: bool, spoken: bool) -> Self {
        let mut block = Self::new(item_id, item, streaming);
        block.spoken = spoken;
        block
    }

    /// The bounded output tail as text (command output is bytes by law;
    /// display is lossy UTF-8). Zero-copy when the tail is valid UTF-8
    /// (efficiency rider #6 — this runs per command block per dirty frame).
    #[must_use]
    pub fn output_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.output_tail)
    }
}

const LARGE_AGENT_MESSAGE_BYTES: usize = 64 * 1024;

fn index_agent_lines(item: &TurnItem) -> Vec<u32> {
    match item {
        TurnItem::AgentMessage { text } => index_agent_reply(text),
        _ => Vec::new(),
    }
}

fn append_agent_line_starts(
    starts: &mut Vec<u32>,
    previous_len: usize,
    delta: &haider_protocol::reply::ReplyText,
) {
    let mut base = previous_len;
    delta.visit_strs(|segment| {
        starts.extend(
            segment
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .filter_map(|(index, _)| u32::try_from(base + index + 1).ok()),
        );
        base = base.saturating_add(segment.len());
    });
}

pub(crate) fn index_agent_reply(text: &haider_protocol::reply::ReplyText) -> Vec<u32> {
    if text.len() <= LARGE_AGENT_MESSAGE_BYTES || text.len() > u32::MAX as usize {
        return Vec::new();
    }
    let mut starts = vec![0];
    let mut base = 0_usize;
    text.visit_strs(|segment| {
        starts.extend(
            segment
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .filter_map(|(index, _)| u32::try_from(base + index + 1).ok()),
        );
        base = base.saturating_add(segment.len());
    });
    starts
}

/// A hidden direct-user-shell provenance marker. Unknown, malformed, and
/// non-item extensions are not display authority.
#[must_use]
pub(crate) fn user_command_origin(
    payload: &EventPayload,
) -> Option<haider_protocol::item::UserCommandOriginV1> {
    let EventPayload::Item(ItemEvent::Completed { item, .. }) = payload else {
        return None;
    };
    haider_protocol::item::UserCommandOriginV1::from_extension_item(item)
}

#[derive(Debug, Clone)]
struct EffectToolOwner {
    item_id: ItemId,
    call_id: String,
}

#[derive(Debug)]
struct PendingEffectFailure {
    owners: Vec<EffectToolOwner>,
    error: String,
}

/// What one raw envelope did to the reducer (W3c3, report R11 cut 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawOutcome {
    /// Reduced (or deliberately invisible); the cursor advanced to this seq.
    Applied,
    /// `seq <= last_applied` — an at-least-once redelivery. Nothing moved.
    Duplicate,
    /// `seq > last_applied + 1`. Reduction STOPPED: nothing was applied and
    /// the cursor did not move. The caller must request a reattach after
    /// `after_seq` before any later envelope may mutate state.
    Gap { after_seq: u64 },
    /// The frame named a different session than the one it was routed to —
    /// rejected without touching anything (report R11 cut 2: "validates
    /// frame session ID").
    WrongSession,
}

/// Who owns an open menu, recorded at `MenuOpened` and consulted when its
/// `MenuAnswered`/`MenuClosed` arrives (report R11 cut 2). Without the map
/// a subagent's answer would land on the session's blocking card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuScopeOwner {
    Session,
    Agent(haider_protocol::ids::AgentId),
}

/// What [`SessionProjection::admit`] decided about one raw envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// In order and `render.ui` — the cursor advanced; apply the payload.
    Apply,
    /// In order but `render.ui == false` — the cursor advanced; the payload
    /// must NOT paint (§6.1).
    Skip,
    /// At-least-once redelivery; the cursor did not move.
    Duplicate,
    /// A hole in the stream; the cursor did not move and nothing may be
    /// applied until the caller reattaches after `after_seq`.
    Gap { after_seq: u64 },
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

fn item_reply_text(item: &TurnItem) -> Option<&haider_protocol::reply::ReplyText> {
    match item {
        TurnItem::AgentMessage { text } | TurnItem::IncompleteAgentMessage { text, .. } => {
            Some(text)
        }
        TurnItem::Reasoning { summary } => Some(summary),
        _ => None,
    }
}

fn item_reply_text_mut(item: &mut TurnItem) -> Option<&mut haider_protocol::reply::ReplyText> {
    match item {
        TurnItem::AgentMessage { text } | TurnItem::IncompleteAgentMessage { text, .. } => {
            Some(text)
        }
        TurnItem::Reasoning { summary } => Some(summary),
        _ => None,
    }
}

/// Session display state, reduced from the envelope stream.
#[derive(Debug, Default)]
pub struct SessionProjection {
    /// Monotonic view-cache invalidation token. The durable/display reducer
    /// remains the authority; render caches only use this to avoid comparing
    /// an unchanged transcript on every animation frame.
    render_revision: u64,
    /// Bumped only when an existing transcript entry changes in place.
    /// Appends deliberately leave this alone, allowing the viewport cache to
    /// preserve measured prefix corrections without mistaking an edit+append
    /// batch for a pure append.
    entry_mutation_revision: u64,
    last_seq: Option<u64>,
    harness: Option<HarnessStatus>,
    run: Option<RunState>,
    /// F2e: whether THIS turn's failure already produced a visible error
    /// line (`RunFailed` or a client-observed rejection). An `Errored`
    /// state with no reported reason synthesizes one — a turn must never
    /// end in a silent ✗.
    run_failure_reported: bool,
    interrupted: bool,
    entries: Vec<TranscriptEntry>,
    /// Entry ordinals for user prompts, maintained at ingest.
    user_entries: Vec<usize>,
    /// Live computer items that can move the owner's screen.
    screen_control_items: std::collections::HashSet<ItemId>,
    /// Unique append authorities for live assistant/reasoning rows. Completed
    /// transcript items retain only their shared reply range.
    reply_writers: std::collections::HashMap<ItemId, haider_protocol::reply::ReplyArenaWriter>,
    /// Item ids whose lifecycle has closed — a re-delivered `Completed` (or a
    /// stale `Started`) for one of these is idempotently ignored (replace
    /// semantics: one item, one block, ever).
    finished_items: std::collections::HashSet<ItemId>,
    /// Results may precede or follow their completed item during attach.
    pending_tool_results: std::collections::HashMap<String, haider_protocol::tool::BoundedResult>,
    /// Effect intents are emitted while their owning tool row is live. A
    /// provider may stream more than one call at once, so retain every live
    /// candidate until a call-id/item-id join and matching error disambiguate
    /// the owner.
    effect_tool_owners: std::collections::HashMap<EffectId, Vec<EffectToolOwner>>,
    /// A failed effect precedes the actor's matching `ToolResult`; defer its
    /// fallback row until the call-id join proves whether the inline row owns
    /// the same failure.
    pending_effect_failures: Vec<PendingEffectFailure>,
    menu: Option<Menu>,
    /// Menu-id → opening scope (report R11 cut 2). Stream-scoped: hydration
    /// starts it empty, and a menu with no recorded opening falls back to
    /// the pre-W3c3 broadcast.
    menu_owner: std::collections::HashMap<haider_protocol::ids::MenuId, MenuScopeOwner>,
    /// The active computer OS-permission grant card (additive
    /// `permission_grant_needed` event). It enriches the paired blocking
    /// `computer-os-permission` menu with the Open Settings / Restart actions
    /// and the native-prompt explanation; cleared by `permission_grant_resolved`.
    permission_card: Option<haider_protocol::permission::PermissionGrantNeeded>,
    todos: Option<TodoPanel>,
    usage: Option<Usage>,
    /// W-G: assistant OUTPUT text characters streamed this turn — the honest
    /// fallback source for the throughput row when the provider reports no
    /// incremental usage. A cheap monotonic counter (bumped on each text
    /// delta), reset at each new turn's start; approximate tokens are derived
    /// as `chars / 4`.
    streamed_output_chars: u64,
    /// Latest durable context-occupancy snapshot (W7b), consumed from the
    /// journal's `context_footprint_v1` extension items — never a
    /// transcript row (one arrives per provider round).
    latest_footprint: Option<haider_protocol::context::ContextFootprint>,
    /// B2b-m3: the durable node → display-entry association, in commit
    /// order — `(entry index, node id)`. Recorded when a `NodeCommitted`
    /// applies (see [`Self::record_node_anchor`]); the `/tree` rows and the
    /// render-resolved jump both look identities up here, never through
    /// text matching. Stream-scoped like the cursor: `hydrate` starts it
    /// empty (the demo store persists no nodes).
    node_entries: Vec<(usize, NodeId)>,
    /// A voice turn is live: blocks started now render ` · ♪ speaking`
    /// (demo-local — set by the driver's Voice beats, never an envelope).
    voice_live: bool,
    /// Per-projection counter minting unique ids for SEEDED rows, so two
    /// sample sessions replayed in a row never collide on a closed item id.
    seed_seq: u64,
    /// M2b: the highest graph attempt-epoch seen so far. A new epoch — the
    /// declared START node reopening graph-wide at a higher attempt — is the
    /// retry-note trigger, generalized from the M1 `node == BUILD` name check.
    /// Reset to 0 on every `GraphPinned` (a fresh instance, incl. a switch).
    graph_epoch: u32,
    // Honesty counters — surfaced, never fatal.
    gap_seen: bool,
    orphan_deltas: u64,
    unknown_payloads: u64,
    duplicate_items: u64,
}

fn is_screen_control_item(item: &TurnItem) -> bool {
    let TurnItem::ToolCall {
        name, status, args, ..
    } = item
    else {
        return false;
    };
    name == "computer"
        && matches!(
            status,
            haider_protocol::item::ToolStatus::InProgress
                | haider_protocol::item::ToolStatus::Pending
        )
        // Observation (screenshot / cursor_position) is not control.
        && !matches!(
            args.get("action").and_then(|value| value.as_str()),
            Some("screenshot") | Some("cursor_position") | None
        )
}

impl SessionProjection {
    /// Apply hidden provenance to an already-visible command block. The
    /// marker is committed after the started item, so no pending side table
    /// is needed; a missing target fails closed as model-origin display.
    #[must_use]
    pub fn mark_user_command(
        &mut self,
        origin: &haider_protocol::item::UserCommandOriginV1,
    ) -> bool {
        let Some(block) = self.entries.iter_mut().find_map(|entry| match entry {
            TranscriptEntry::Item(block) if block.item_id == origin.command_item_id => Some(block),
            _ => None,
        }) else {
            return false;
        };
        if !matches!(
            &block.item,
            TurnItem::CommandExecution { call_id, .. } if call_id == &origin.call_id
        ) {
            return false;
        }
        let changed = !block.user_command;
        block.user_command = true;
        if changed {
            self.render_revision = self.render_revision.wrapping_add(1);
            self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
        }
        true
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a projection from persisted display state (the demo store's
    /// load — see `crate::demo_store`). Only what the sim persists comes
    /// back: transcript rows, the open menu, the todo panel, the usage
    /// meter and the idle(i) marker. Everything stream-scoped starts
    /// fresh — `run`/`harness` are `None` (every session loads IDLE, sim
    /// load §6), `voice_live` is off, and the idempotency bookkeeping
    /// (`finished_items`, seq accounting) is empty: no in-flight delivery
    /// survives a restart, and a NEW turn's ids must never be swallowed as
    /// duplicates of rows restored from disk.
    #[must_use]
    pub fn hydrate(
        entries: Vec<TranscriptEntry>,
        menu: Option<Menu>,
        todos: Option<TodoPanel>,
        usage: Option<Usage>,
        interrupted: bool,
    ) -> Self {
        let user_entries = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                matches!(entry, TranscriptEntry::User { .. }).then_some(index)
            })
            .collect();
        let screen_control_items = entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Item(block) if is_screen_control_item(&block.item) => {
                    Some(block.item_id.clone())
                }
                _ => None,
            })
            .collect();
        Self {
            entries,
            user_entries,
            screen_control_items,
            menu,
            todos,
            usage,
            interrupted,
            ..Self::default()
        }
    }

    /// Consume one raw envelope in stream order — the SOLE cursor authority
    /// (W3c3, report R11 cut 2).
    ///
    /// STRICT gap law: `seq > last_applied + 1` applies NOTHING and leaves
    /// the cursor where it was, so the caller can reattach after the last
    /// FULLY APPLIED sequence before any later envelope mutates state. The
    /// pre-W3c3 reducer recorded the gap and kept going, which silently
    /// projected a hole in history; the store is the lag buffer (R9), so
    /// papering over a gap is never the client's job.
    ///
    /// Duplicate seqs are skipped (delivery is at-least-once). Unknown
    /// payloads are counted and ignored (forward-compat law). Envelopes
    /// marked `render.ui == false` advance the cursor but never mutate
    /// display state (§6.1: three surfaces, never conflated).
    pub fn apply_raw(&mut self, envelope: &RawEnvelope) -> RawOutcome {
        match self.admit(envelope) {
            Admission::Apply => {
                self.apply_payload_json(&envelope.payload);
                RawOutcome::Applied
            }
            Admission::Skip => RawOutcome::Applied,
            Admission::Duplicate => RawOutcome::Duplicate,
            Admission::Gap { after_seq } => RawOutcome::Gap { after_seq },
        }
    }

    /// The cursor gate ALONE — the strict half of [`Self::apply_raw`],
    /// separated so a router can decide WHERE the payload lands (a session's
    /// own projection, or a subagent chip's transcript) while the cursor
    /// stays single-authority here. The cursor advances on `Apply`/`Skip`
    /// and never on `Duplicate`/`Gap`.
    pub fn admit(&mut self, envelope: &RawEnvelope) -> Admission {
        if let Some(last) = self.last_seq {
            if envelope.seq <= last {
                return Admission::Duplicate;
            }
            if envelope.seq != last + 1 {
                self.gap_seen = true;
                return Admission::Gap { after_seq: last };
            }
        }
        self.last_seq = Some(envelope.seq);
        if envelope.render.ui {
            Admission::Apply
        } else {
            Admission::Skip
        }
    }

    /// Decode one admitted payload into this projection; an undecodable kind
    /// is counted, never fatal (forward-compat law).
    pub fn apply_payload_json(&mut self, payload: &serde_json::Value) {
        match serde_json::from_value::<EventPayload>(payload.clone()) {
            Ok(payload) => self.apply(&payload),
            Err(_) => self.unknown_payloads += 1,
        }
    }

    /// Count one payload this build cannot decode (forward-compat law) —
    /// the router's hook when IT owns the decode.
    pub fn count_unknown_payload(&mut self) {
        self.unknown_payloads += 1;
    }

    /// Record which scope opened a menu (report R11 cut 2).
    pub fn note_menu_owner(&mut self, menu: haider_protocol::ids::MenuId, owner: MenuScopeOwner) {
        self.menu_owner.insert(menu, owner);
    }

    /// The recorded opening scope of a menu, if this stream carried it.
    #[must_use]
    pub fn menu_owner(&self, menu: &haider_protocol::ids::MenuId) -> Option<&MenuScopeOwner> {
        self.menu_owner.get(menu)
    }

    /// The greatest FULLY APPLIED sequence — the only reattach cursor there
    /// is (R9's cursor law: server telemetry like `last_queued_seq` is never
    /// resume authority). `None` before the first envelope.
    #[must_use]
    pub const fn last_applied(&self) -> Option<u64> {
        self.last_seq
    }

    /// Seed the cursor at attach time so the FIRST delivered envelope is
    /// gap-checked too: a client that attached `after_seq = N` and receives
    /// `N + 3` has lost `N+1..=N+2` and must reattach, not paint.
    pub fn set_last_applied(&mut self, after_seq: u64) {
        self.last_seq = Some(after_seq);
    }

    /// Consume one typed payload (used by tests and the mock client).
    pub fn apply(&mut self, payload: &EventPayload) {
        self.render_revision = self.render_revision.wrapping_add(1);
        match payload {
            // 954 queue deltas: consumed by the composer's queue panel
            // (next workstream); until it lands the projection observes
            // and deliberately holds no state — an explicit arm so the
            // exhaustive match forces the panel author (me) to wire it.
            EventPayload::QueueChanged(_) => {}
            EventPayload::HarnessStatus(status) => self.harness = Some(status.clone()),
            EventPayload::SessionState(state) => {
                if let haider_protocol::state::SessionState::Idle { interrupted } = state {
                    self.interrupted = *interrupted;
                }
            }
            EventPayload::RunState(run) => {
                if run.is_terminal() {
                    self.flush_pending_effect_failures();
                    self.effect_tool_owners.clear();
                    // The OS-permission grant card is TURN-SCOPED: it exists
                    // to enrich a blocking menu that parks the current turn
                    // ("grant it, then Retry — it resumes automatically").
                    // Its only other exit is a matching
                    // `permission_grant_resolved`, which a CANCELLED turn
                    // never produces — so without this the card outlived its
                    // turn and sat over an idle session offering a Retry that
                    // had nothing left to resume.
                    self.permission_card = None;
                }
                // W-G: a genuine turn OPENING (idle/none → non-terminal) resets
                // the streamed-output char tally so the throughput fallback
                // starts each turn from zero — a mid-turn RunState update
                // (Streaming → RunningTool → Streaming) must NOT reset it.
                let was_idle = self.run.as_ref().is_none_or(RunState::is_terminal);
                if was_idle && !run.is_terminal() {
                    self.streamed_output_chars = 0;
                }
                if matches!(run, RunState::Cancelled) {
                    self.interrupted = true;
                } else if !run.is_terminal() {
                    // A new or progressing turn clears the idle(i) marker —
                    // and re-arms the F2e unpaired-error synthesizer.
                    self.interrupted = false;
                    self.run_failure_reported = false;
                }
                // F2e: an `Errored` turn with NO paired `RunFailed` reason
                // still gets a visible line — the pre-W5g-6 owner bug
                // (badge-only ✗) must stay dead even when the daemon
                // serves no public reason.
                if matches!(run, RunState::Errored) && !self.run_failure_reported {
                    self.run_failure_reported = true;
                    self.entries.push(TranscriptEntry::Error {
                        text: "errored — the daemon reported no public reason".to_owned(),
                        presentation: None,
                    });
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
            EventPayload::MenuClosed { menu, .. } => {
                if self.menu.as_ref().is_some_and(|open| open.id == *menu) {
                    self.menu = None;
                }
            }
            EventPayload::UserMessage {
                text, attachments, ..
            } => {
                self.user_entries.push(self.entries.len());
                self.entries.push(TranscriptEntry::User {
                    text: text.clone(),
                    attachments: attachments.len(),
                    voice: false,
                    from_main: false,
                });
            }
            EventPayload::PeerMessage(message) => self.push_peer_message(
                message.msg_id.clone(),
                message.from.name.clone(),
                peer_kind_label(message.from.kind).to_owned(),
                message.message.clone(),
            ),
            EventPayload::Item(event) => self.apply_item(event),
            EventPayload::ToolResult { call_id, result } => self.apply_tool_result(call_id, result),
            EventPayload::Usage(usage) => self.usage = Some(usage.clone()),
            // The failed run's PUBLIC reason joins the transcript (W5g-6):
            // the envelope always carried it; only the badge ever showed.
            EventPayload::RunFailed {
                code,
                message,
                presentation,
                ..
            } => {
                self.run_failure_reported = true;
                self.entries.push(TranscriptEntry::Error {
                    text: presentation.as_ref().map_or_else(
                        || format!("{} — {message}", code.as_str()),
                        format_error_presentation,
                    ),
                    presentation: presentation.clone(),
                });
            }
            EventPayload::ClientDiagnostic { code, message, .. } => {
                self.entries.push(TranscriptEntry::Error {
                    text: format!("{code} — {message}"),
                    presentation: Some(haider_protocol::error::ErrorPresentation::new(
                        code,
                        "Client/daemon incompatible — update",
                        message,
                        haider_protocol::error::ErrorScope::Session,
                        [haider_protocol::error::ErrorAction::None],
                    )),
                });
            }
            // B2b-m3: a committed node ANCHORS its display entry — never a
            // transcript row of its own (the sim's tree reads entries; the
            // node is the durable identity riding beside them).
            EventPayload::NodeCommitted(node) => self.record_node_anchor(node),
            // F2e error-visibility sweep: every turn-level failure the
            // wire can carry surfaces as a VISIBLE session-view line with
            // its public reason — never a silent IDLE.
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Intent(intent)) => {
                let owners = self.live_tool_owners();
                if !owners.is_empty() {
                    self.effect_tool_owners
                        .insert(intent.effect.clone(), owners);
                }
            }
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                effect,
                outcome,
                ..
            }) => {
                use haider_protocol::effect::EffectOutcome;
                let owner = self.effect_tool_owners.remove(effect);
                match outcome {
                    EffectOutcome::Failed { error } => {
                        if let Some(owners) = owner {
                            if !self.effect_failure_is_inline(&owners, error) {
                                self.pending_effect_failures.push(PendingEffectFailure {
                                    owners,
                                    error: error.clone(),
                                });
                            }
                        } else {
                            self.push_effect_failure(error);
                        }
                    }
                    EffectOutcome::CancelledEscalated { note } => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!("effect cancel escalated — {note}"),
                            presentation: None,
                        });
                    }
                    EffectOutcome::Unknown => {
                        self.entries.push(TranscriptEntry::Error {
                            text: "effect outcome unknown — crash window; reconcile via the recovery menu"
                                .to_owned(),
                            presentation: None,
                        });
                    }
                    EffectOutcome::Ok | EffectOutcome::Cancelled => {}
                }
            }
            EventPayload::GateReport(report) => {
                use haider_protocol::verify::VerifyVerdict;
                let new_errors = report.new_errors.len();
                match &report.verdict {
                    VerifyVerdict::ErroredWithReport => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!(
                                "verify errored — cycle cap exhausted · {new_errors} new error(s)"
                            ),
                            presentation: None,
                        });
                    }
                    VerifyVerdict::FailedEnv { item } => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!("verify failed-env — {item}"),
                            presentation: None,
                        });
                    }
                    VerifyVerdict::Incomplete { reason } => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!("verify incomplete — {reason}"),
                            presentation: None,
                        });
                    }
                    VerifyVerdict::AcknowledgedRed => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!(
                                "verify acknowledged-red — {new_errors} new error(s) cited out of scope"
                            ),
                            presentation: None,
                        });
                    }
                    _ => {}
                }
            }
            // §4.4: a rotation surfaces like a model change — a visible
            // note naming the new account and the public cause.
            EventPayload::Rotation(rotation) => {
                use haider_protocol::credential::RotationCause;
                let cause = match rotation.cause {
                    RotationCause::RateLimit => "rate limit",
                    RotationCause::Error => "provider error",
                    RotationCause::Manual => "manual",
                };
                self.push_note(format!(
                    "account rotated → {} ({} · {cause})",
                    rotation.to.as_str(),
                    rotation.provider
                ));
            }
            EventPayload::LockdownRefused(refusal) => {
                self.entries.push(TranscriptEntry::Refusal {
                    provider: refusal.provider.clone(),
                    tool: refusal.tool.clone(),
                    reason: refusal.reason.clone(),
                });
            }
            EventPayload::LockdownQuota(_) => {}
            EventPayload::ProviderTrustChanged(change) => self.push_note(format!(
                "provider trust changed · {} → {}",
                change.provider, change.trust
            )),
            EventPayload::ProviderAuthChanged(change) => self.push_note(format!(
                "provider auth changed · {} → {}",
                change.provider, change.auth_requirement
            )),
            // Consumed by later waves (effects timeline, subagent tree,
            // accounts). The projection stays tolerant of them now.
            EventPayload::Effect(_)
            | EventPayload::AgentSpawned(_)
            | EventPayload::AgentReport(_)
            | EventPayload::AgentChipState { .. } => {}
            // Convergence Graph M1: the live strip and status view render the
            // daemon's reduction; the transcript keeps quiet `·` note rows so
            // the durable convergence story reads in scrollback too. State
            // changes only — forward advancement and first-attempt opens are
            // the strip's job, so we skip them here to keep scrollback calm.
            EventPayload::GraphPinned(pinned) => {
                // A fresh instance (initial pin or a switch): reset the epoch
                // watermark so the new graph's first retry notes correctly.
                self.graph_epoch = 0;
                self.push_note(format!(
                    "⚑ {} pinned · {}",
                    pinned.template,
                    graph_digest_short(&pinned.digest)
                ));
            }
            EventPayload::GraphAttemptOpened(opened) => {
                // M2b: the START node opens first at each new epoch, so the
                // first attempt above the watermark is the graph-wide retry.
                // Note it once (never on the first epoch); downstream opens at
                // the same epoch stay quiet.
                if opened.attempt > self.graph_epoch {
                    self.graph_epoch = opened.attempt;
                    if opened.attempt > 1 {
                        self.push_note(format!(
                            "{} attempt {}/{} — earlier greens are stale",
                            opened.node.label(),
                            opened.attempt,
                            haider_protocol::graph::GRAPH_MAX_ATTEMPTS
                        ));
                    }
                }
            }
            EventPayload::EvidenceRecorded(evidence) => {
                use haider_protocol::graph::EvidenceVerdict;
                let verdict = match evidence.verdict {
                    EvidenceVerdict::Green => "green",
                    EvidenceVerdict::Red => "red",
                };
                self.push_note(format!(
                    "evidence · {} {verdict} — {}",
                    evidence.node.label(),
                    graph_detail_fragment(&evidence.detail)
                ));
            }
            EventPayload::GraphGateSatisfied(satisfied) => {
                self.push_note(format!("{} gate satisfied", satisfied.node.label()));
            }
            EventPayload::GraphBlocked(blocked) => {
                self.push_note(format!(
                    "ship-loop blocked — {} at {}",
                    graph_block_reason(blocked.reason),
                    blocked.node.label()
                ));
            }
            EventPayload::GraphCompleted(_) => {
                self.push_note("✓ ship-loop complete · every gate satisfied".to_owned());
            }
            EventPayload::GraphAbandoned(abandoned) => {
                self.push_note(format!(
                    "ship-loop abandoned — {}",
                    graph_detail_fragment(&abandoned.why)
                ));
            }
            EventPayload::GraphAdvanced(_) => {}
            // M2a: a daemon-observed process signal is evidence PLUMBING — the
            // durable exit/arg/transcript provenance that a DaemonVerified slot
            // references. The convergence story surfaces through the
            // `EvidenceRecorded` note it backs (and the inspect screen), so the
            // transcript stays quiet here, like `GraphAdvanced`.
            EventPayload::ProcessSignalRecorded(_) => {}
            // M2b: a node became ready (its deps are satisfied) in the
            // dependency-driven engine — forward position is the strip's job,
            // so the transcript stays quiet, like `GraphAdvanced`.
            EventPayload::GraphNodeReadied(_) => {}
            // M2b: the pinned workflow was replaced by a mid-run switch. A new
            // `GraphPinned` note follows; this records the supersession itself.
            EventPayload::GraphSuperseded(_) => {
                self.push_note(
                    "⇄ workflow switched — the previous graph was superseded".to_owned(),
                );
            }
            // M2c: a final answer was DEFERRED because the active graph still
            // has unmet obligations. The guardrail never silently drops — the
            // model keeps working or explicitly abandons via the menu.
            EventPayload::GraphFinalizationDeferred(deferred) => {
                let count = deferred.unmet_nodes.len();
                let names = deferred
                    .unmet_nodes
                    .iter()
                    .take(3)
                    .map(haider_protocol::graph::GraphNodeName::label)
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = count.saturating_sub(3);
                let more = if more > 0 {
                    format!(" +{more}")
                } else {
                    String::new()
                };
                self.push_note(format!(
                    "⚠ finalize deferred — {count} unmet ({names}{more}) · keep working or abandon the graph"
                ));
            }
            // M2d: a per-todo workflow run-set opened over the current plan —
            // one aggregate note; the strip/status view carries completed/K.
            EventPayload::GraphRunSetOpened(opened) => {
                self.push_note(format!(
                    "⚑ run-set opened — {} todo workflow(s)",
                    opened.required_children
                ));
            }
            // M2d: one child graph bound to a todo — quiet (the run-set note +
            // the aggregate render carry the story), like `GraphNodeReadied`.
            EventPayload::TodoGraphAttached(_) => {}
            // M2e: a subagent was granted its own workflow (SPARSE by design —
            // the decision gate defaults to a bare attempt), worth one note.
            EventPayload::ChildGraphAttached(_) => {
                self.push_note("⚑ subagent workflow attached".to_owned());
            }
            // M2e: child-template cache bookkeeping (observation → promotion) is
            // internal — quiet until a render surfaces the reusable badge.
            EventPayload::ChildTemplateObserved(_) | EventPayload::ChildTemplatePromoted(_) => {}
            // Checkpoint facts power the explicit /checkpoints surface; raw
            // replay must not fabricate an extra transcript row for them.
            EventPayload::CheckpointRecorded(_) => {}
        }
    }

    fn live_tool_owners(&self) -> Vec<EffectToolOwner> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Item(block) if block.streaming => match &block.item {
                    TurnItem::ToolCall { call_id, .. } => Some(EffectToolOwner {
                        item_id: block.item_id.clone(),
                        call_id: call_id.clone(),
                    }),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn effect_failure_is_inline(&mut self, owners: &[EffectToolOwner], error: &str) -> bool {
        let inline = self.entries.iter_mut().any(|entry| {
            let TranscriptEntry::Item(block) = entry else {
                return false;
            };
            let TurnItem::ToolCall {
                call_id, status, ..
            } = &block.item
            else {
                return false;
            };
            if !owners
                .iter()
                .any(|owner| owner.item_id == block.item_id && owner.call_id == *call_id)
                || !tool_status_carries_effect_failure(*status)
            {
                return false;
            }
            if let Some(reason) = &block.tool_reason {
                return reason.contains(&bounded_effect_error(error));
            }
            if block.streaming {
                return false;
            }
            block.tool_reason = Some(bounded_effect_error(error));
            true
        });
        self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
        inline
    }

    fn push_effect_failure(&mut self, error: &str) {
        self.entries.push(TranscriptEntry::Error {
            text: format!("effect failed — {error}"),
            presentation: None,
        });
    }

    fn flush_pending_effect_failures(&mut self) {
        let failures = std::mem::take(&mut self.pending_effect_failures);
        for failure in failures {
            self.push_effect_failure(&failure.error);
        }
    }

    /// Removes one settled row from every remaining pending failure's
    /// candidate list: a row that already resolved one effect cannot own
    /// another, so a later-settling row — not this one — resolves the rest.
    fn retire_effect_owner_row(
        failures: &mut [PendingEffectFailure],
        item_id: &ItemId,
        call_id: &str,
    ) {
        for failure in failures {
            failure
                .owners
                .retain(|owner| !(owner.item_id == *item_id && owner.call_id == call_id));
        }
    }

    fn settle_effect_failures_for_result(
        &mut self,
        item_id: &ItemId,
        call_id: &str,
        result: &haider_protocol::tool::BoundedResult,
    ) {
        // Core dispatches provider tool calls serially at ToolCallEnd, so one
        // settling result resolves AT MOST ONE pending effect failure. A
        // matching error text is the strongest join evidence and wins over
        // arrival order; otherwise the documented first-settling candidate
        // law picks the oldest candidate. Either way the settled row retires
        // from the remaining failures' candidate lists, so a mismatched-error
        // row never settles (or adopts) a second, foreign effect's text.
        // Unresolved failures keep the no-silent-swallow law via the
        // terminal-state flush.
        let mut failures = std::mem::take(&mut self.pending_effect_failures);
        let is_owner_row = |failure: &PendingEffectFailure| {
            failure
                .owners
                .iter()
                .any(|owner| owner.item_id == *item_id && owner.call_id == call_id)
        };
        let matched = tool_result_status_carries_effect_failure(result.status)
            .then(|| {
                failures.iter().position(|failure| {
                    is_owner_row(failure)
                        && tool_result_carries_effect_error(result, &failure.error)
                })
            })
            .flatten();
        let selected = matched.or_else(|| failures.iter().position(is_owner_row));
        if let Some(index) = selected {
            let failure = failures.remove(index);
            if matched.is_none() {
                self.push_effect_failure(&failure.error);
            }
            Self::retire_effect_owner_row(&mut failures, item_id, call_id);
        }
        self.pending_effect_failures = failures;
    }

    fn settle_effect_failures_for_completed_tool(
        &mut self,
        item_id: &ItemId,
        call_id: &str,
        status: haider_protocol::item::ToolStatus,
    ) {
        // Same one-settle-per-row law as the result door. Containment of an
        // effect's error in the row's existing reason is the strongest join
        // evidence; adoption (stamping the effect's error onto the row) is
        // allowed only into a row that carries NO error text of its own — a
        // mismatched-error row never adopts a foreign effect's text.
        let mut failures = std::mem::take(&mut self.pending_effect_failures);
        let is_owner_row = |failure: &PendingEffectFailure| {
            failure
                .owners
                .iter()
                .any(|owner| owner.item_id == *item_id && owner.call_id == call_id)
        };
        let carries_failure = tool_status_carries_effect_failure(status);
        let row_reason = self.entries.iter().rev().find_map(|entry| match entry {
            TranscriptEntry::Item(block) if block.item_id == *item_id => {
                Some(block.tool_reason.clone())
            }
            _ => None,
        });
        let matched = carries_failure
            .then(|| {
                row_reason
                    .as_ref()
                    .and_then(Option::as_ref)
                    .and_then(|reason| {
                        failures.iter().position(|failure| {
                            is_owner_row(failure)
                                && reason.contains(&bounded_effect_error(&failure.error))
                        })
                    })
            })
            .flatten();
        let selected = matched.or_else(|| failures.iter().position(is_owner_row));
        if let Some(index) = selected {
            let failure = failures.remove(index);
            let mut inline = matched.is_some();
            if !inline && carries_failure && matches!(row_reason, Some(None)) {
                // The row failed carrying no reason of its own: the
                // first-settling candidate law lets it adopt the oldest
                // candidate's error.
                inline = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::Item(block) if block.item_id == *item_id => {
                            if block.tool_reason.is_none() {
                                block.tool_reason = Some(bounded_effect_error(&failure.error));
                                Some(true)
                            } else {
                                Some(false)
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or(false);
                if inline {
                    self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
                }
            }
            if !inline {
                self.push_effect_failure(&failure.error);
            }
            Self::retire_effect_owner_row(&mut failures, item_id, call_id);
        }
        self.pending_effect_failures = failures;
    }

    fn apply_tool_result(&mut self, call_id: &str, result: &haider_protocol::tool::BoundedResult) {
        let reason = bounded_tool_reason(result);
        let item = self.entries.iter_mut().rev().find_map(|entry| match entry {
            TranscriptEntry::Item(block) => match &mut block.item {
                TurnItem::ToolCall {
                    call_id: known,
                    status,
                    ..
                } if known == call_id => {
                    *status = result.status.item_status();
                    block.tool_reason = reason.clone();
                    Some((block.item_id.clone(), is_screen_control_item(&block.item)))
                }
                _ => None,
            },
            _ => None,
        });
        if let Some((item_id, screen_control_active)) = item {
            self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
            if screen_control_active {
                self.screen_control_items.insert(item_id.clone());
            } else {
                self.screen_control_items.remove(&item_id);
            }
            self.settle_effect_failures_for_result(&item_id, call_id, result);
        } else {
            self.pending_tool_results
                .insert(call_id.to_owned(), result.clone());
        }
    }

    /// F2a/F2e: a CLIENT-observed failure joins the transcript as a
    /// visible error line (a rejected command on this session, a typed
    /// selection refusal landing after its picker closed) — never a
    /// silent IDLE. Local display truth only: nothing durable claims it.
    pub fn record_local_error(&mut self, text: String) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.entries.push(TranscriptEntry::Error {
            text,
            presentation: None,
        });
    }

    /// E8 visual pass: a client-observed failure that carries its TYPED
    /// presentation (the busy-retry-exhausted card) records both — the
    /// flattened text stays the plain/greppable authority while the styled
    /// renderer gives the row the same card-shaped err treatment a typed
    /// run failure gets.
    pub(crate) fn record_local_error_card(
        &mut self,
        presentation: haider_protocol::error::ErrorPresentation,
    ) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.entries.push(TranscriptEntry::Error {
            text: format_error_presentation(&presentation),
            presentation: Some(presentation),
        });
    }

    /// B2b-m3: associate one committed node with the display entry it
    /// stands for. The daemon commits a turn's `UserMessage` and its
    /// `NodeCommitted` adjacently in ONE acceptance transaction (compaction
    /// likewise: item completed, then node), so at the moment the node
    /// event applies its display entry is the LAST matching entry — a
    /// stream-order association inside the same atomic batch, never text
    /// matching or cross-batch adjacency inference (research §Q3). A node
    /// kind with no display row records NOTHING: an anchor is never
    /// guessed, and an already-anchored entry is never re-bound.
    fn record_node_anchor(&mut self, node: &haider_protocol::history::TreeNode) {
        if self
            .node_entries
            .iter()
            .any(|(_, known)| known == &node.node)
        {
            return; // replayed fact — one anchor, ever
        }
        let entry = match &node.kind {
            haider_protocol::history::NodeKind::UserTurn { .. } => self
                .entries
                .iter()
                .rposition(|entry| matches!(entry, TranscriptEntry::User { .. })),
            haider_protocol::history::NodeKind::PeerTurn { .. } => self
                .entries
                .iter()
                .rposition(|entry| matches!(entry, TranscriptEntry::Peer { .. })),
            haider_protocol::history::NodeKind::Compaction { .. } => {
                self.entries.iter().rposition(|entry| {
                    matches!(
                        entry,
                        TranscriptEntry::Item(block)
                            if matches!(block.item, TurnItem::ContextCompaction { .. })
                    )
                })
            }
            _ => None,
        };
        if let Some(entry) = entry
            && !self
                .node_entries
                .iter()
                .any(|(anchored, _)| *anchored == entry)
        {
            self.node_entries.push((entry, node.node.clone()));
        }
    }

    /// The display entry anchoring `node`, if this stream committed one
    /// (B2b-m3 — the render-resolved jump looks up here; a missing anchor
    /// keeps the pending jump armed, it never guesses another entry).
    #[must_use]
    pub fn entry_of_node(&self, node: &NodeId) -> Option<usize> {
        self.node_entries
            .iter()
            .find(|(_, known)| known == node)
            .map(|(entry, _)| *entry)
    }

    /// The node anchored at display entry `index` (the `/tree` typed rows).
    #[must_use]
    pub fn node_of_entry(&self, index: usize) -> Option<&NodeId> {
        self.node_entries
            .iter()
            .find(|(entry, _)| *entry == index)
            .map(|(_, node)| node)
    }

    fn apply_item(&mut self, event: &ItemEvent) {
        if self.consume_context_extension(event) {
            return;
        }
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
                    if is_screen_control_item(item) {
                        self.screen_control_items.insert(item_id.clone());
                    }
                    if let Some(text) = item_reply_text(item) {
                        let mut writer = haider_protocol::reply::ReplyArenaWriter::new();
                        let _ = writer.append_shared(text);
                        self.reply_writers.insert(item_id.clone(), writer);
                    }
                    self.entries
                        .push(TranscriptEntry::Item(ItemBlock::new_spoken(
                            item_id.clone(),
                            item.clone(),
                            true,
                            self.voice_live,
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
                    let mut completed_item = item.clone();
                    if let Some(writer) = self.reply_writers.get(item_id) {
                        let canonical = writer.snapshot();
                        if let Some(completed) = item_reply_text_mut(&mut completed_item)
                            && *completed == canonical
                        {
                            *completed = canonical;
                        }
                    }
                    if is_screen_control_item(&completed_item) {
                        self.screen_control_items.insert(item_id.clone());
                    } else {
                        self.screen_control_items.remove(item_id);
                    }
                    self.finished_items.insert(item_id.clone());
                    if let Some(block) = self.open_block_mut(item_id) {
                        // Replace semantics: the final item is authoritative.
                        block.agent_line_starts = index_agent_lines(&completed_item);
                        block.item = completed_item;
                        block.streaming = false;
                        // The completed item carries the parsed args; the raw
                        // fragment accumulation is a duplicate — release it
                        // (efficiency rider #3).
                        block.args_fragments = String::new();
                    } else {
                        // Attach-mid-stream tolerance: a Completed we never
                        // saw start still lands as a finished block.
                        self.entries
                            .push(TranscriptEntry::Item(ItemBlock::new_spoken(
                                item_id.clone(),
                                completed_item,
                                false,
                                self.voice_live,
                            )));
                    }
                    self.reply_writers.remove(item_id);
                }
                if let TurnItem::ToolCall {
                    call_id, status, ..
                } = item
                {
                    if let Some(result) = self.pending_tool_results.remove(call_id) {
                        self.apply_tool_result(call_id, &result);
                    }
                    self.settle_effect_failures_for_completed_tool(item_id, call_id, *status);
                }
            }
        }
    }

    fn apply_delta(&mut self, item_id: &ItemId, delta: &ItemDelta) {
        // W-G: GENERATED text characters that land on the open block, tallied
        // AFTER the block borrow ends (the counter and the block are both
        // fields of `self`). Reasoning and tool-call arguments ARE provider
        // output — the model generates (and the provider meters) them — so
        // they count toward the throughput rate; without them the readout
        // flatlined at 0 through thinking- and tool-heavy turns (owner bug
        // 2026-08-15). Command OUTPUT is tool-execution data, not generation,
        // and stays excluded.
        let mut output_chars = 0u64;
        let reply_snapshot = match delta {
            ItemDelta::Text { text } | ItemDelta::Reasoning { text } => {
                output_chars = u64::try_from(text.char_count()).unwrap_or(u64::MAX);
                self.reply_writers.get_mut(item_id).map(|writer| {
                    let _ = writer.append_shared(text);
                    writer.snapshot()
                })
            }
            _ => None,
        };
        {
            let Some(block) = self.open_block_mut(item_id) else {
                self.orphan_deltas += 1;
                return;
            };
            match delta {
                ItemDelta::Text { text } => {
                    if let TurnItem::AgentMessage { text: body } = &mut block.item {
                        let previous_len = body.len();
                        if let Some(snapshot) = reply_snapshot.clone() {
                            *body = snapshot;
                        } else {
                            *body = text.clone();
                        }
                        if body.len() > LARGE_AGENT_MESSAGE_BYTES {
                            if block.agent_line_starts.is_empty() {
                                block.agent_line_starts = index_agent_reply(body);
                            } else {
                                append_agent_line_starts(
                                    &mut block.agent_line_starts,
                                    previous_len,
                                    text,
                                );
                            }
                        }
                    }
                }
                ItemDelta::Reasoning { text } => {
                    if let TurnItem::Reasoning { summary } = &mut block.item {
                        if let Some(snapshot) = reply_snapshot {
                            *summary = snapshot;
                        } else {
                            *summary = text.clone();
                        }
                    }
                }
                ItemDelta::ToolArgs { fragment } => {
                    block.args_fragments.push_str(fragment);
                    output_chars = fragment.chars().count() as u64;
                }
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
        self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
        self.streamed_output_chars = self.streamed_output_chars.saturating_add(output_chars);
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

    /// True while the run is in its THINKING beat SPECIFICALLY. This is the
    /// narrow beat predicate — it is NOT the tail-indicator gate (that is
    /// [`Self::is_turn_active`]). Its remaining caller is the provider-open
    /// progress readout, which is only meaningful before the response stream
    /// opens; once the provider is streaming there is nothing left to wait on.
    #[must_use]
    pub const fn is_thinking(&self) -> bool {
        matches!(self.run, Some(RunState::Thinking))
    }

    /// True while a turn is ACTIVELY RUNNING — the gate for the transcript-tail
    /// `● thinking…` indicator (owner report: the indicator vanished the moment
    /// the run left the Thinking beat, so a visibly `▮ STREAMING` turn showed
    /// nothing above the composer; it must be up for the whole run).
    ///
    /// Derived from [`Self::badge_tone`] ON PURPOSE rather than from a second
    /// hand-written variant list: the badge already classifies every
    /// `RunState`, and two parallel taxonomies would drift the first time a
    /// variant is added. A new state is therefore classified in exactly one
    /// place, and the exhaustive match there is the compiler's reminder.
    ///
    /// Shows for the Active / Tool / Compacting groups (`Thinking`,
    /// `Streaming`, `Concluding`, `Verifying`, `RunningTool`, `Cancelling`,
    /// `Compacting`). Deliberately EXCLUDED:
    /// * Idle — `None`/`Done`/`Cancelled`: nothing is running.
    /// * Restful — `Queued`/`Waiting`/`Retrying`: the owner excluded waiting,
    ///   and `Retrying` already owns a dedicated tail row (`retrying_line`),
    ///   so including it would stack two indicators at the same tail.
    /// * Attention — `InputRequired`/`PermissionRequired`: blocked on the
    ///   user, with a menu already on screen.
    /// * `EffectUnknown` / `Error`: terminal honesty states, not work.
    ///
    /// Also false while the harness is `Starting` (the badge's own early
    /// return), which keeps the boot screen's animation the only one running.
    #[must_use]
    pub fn is_turn_active(&self) -> bool {
        matches!(
            self.badge_tone(),
            BadgeTone::Active | BadgeTone::Tool | BadgeTone::Compacting
        )
    }

    /// W-G: true while the turn is actively producing OUTPUT — `Streaming`
    /// (assistant text) or `RunningTool` (a tool call mid-turn). The
    /// throughput row shows only in these states; every other state (idle,
    /// thinking, waiting, terminal) hides it so idle frames cost nothing.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        matches!(self.run, Some(RunState::Streaming | RunState::RunningTool))
    }

    /// W-G: an APPROXIMATE output-token count for this turn, derived from the
    /// streamed assistant-text characters (~4 chars per token). Used only as
    /// the honest fallback when the provider reports no incremental usage —
    /// the readout is marked `~` when it feeds the tracker.
    #[must_use]
    pub const fn streamed_output_tokens_approx(&self) -> u64 {
        self.streamed_output_chars / 4
    }

    /// M4: the visible retry view — `(attempt, max, delay_ms)` while the run
    /// is backing off after a retryable provider failure, so the transcript
    /// tail can render `✻ API error · Retrying in Ns · attempt K/max`. `None`
    /// in every other state.
    #[must_use]
    pub const fn retrying(&self) -> Option<(u32, u32, u64)> {
        if let Some(RunState::Retrying {
            attempt,
            max,
            delay_ms,
            ..
        }) = self.run
        {
            Some((attempt, max, delay_ms))
        } else {
            None
        }
    }

    /// True while the idle(i) marker is set (esc interrupted the last turn).
    #[must_use]
    pub const fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Append a display-only note row (sim `NoteRow`): auto-title,
    /// interrupt, mid-turn input echoes. Never sourced from envelopes.
    pub fn push_note(&mut self, text: String) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.entries.push(TranscriptEntry::Note { text });
    }

    /// Append one peer-message block from daemon event truth.
    pub fn push_peer_message(
        &mut self,
        msg_id: String,
        sender: String,
        sender_kind: String,
        text: String,
    ) {
        if self.has_peer_message(&msg_id) {
            return;
        }
        self.render_revision = self.render_revision.wrapping_add(1);
        self.entries.push(TranscriptEntry::Peer {
            msg_id,
            sender,
            sender_kind,
            text,
            receipt: None,
        });
    }

    /// Attach an optional delivery receipt to an existing peer block.
    pub fn set_peer_receipt(&mut self, msg_id: &str, receipt: PeerDelivery) {
        let Some(entry) = self.entries.iter_mut().rev().find(|entry| {
            matches!(entry, TranscriptEntry::Peer { msg_id: existing, .. } if existing == msg_id)
        }) else {
            return;
        };
        if let TranscriptEntry::Peer {
            receipt: current, ..
        } = entry
            && *current != Some(receipt)
        {
            *current = Some(receipt);
            self.render_revision = self.render_revision.wrapping_add(1);
            self.entry_mutation_revision = self.entry_mutation_revision.wrapping_add(1);
        }
    }

    fn has_peer_message(&self, msg_id: &str) -> bool {
        self.entries.iter().rev().any(|entry| {
            matches!(entry, TranscriptEntry::Peer { msg_id: existing, .. } if existing == msg_id)
        })
    }

    /// Append a voice user row (sim /say + talk: ◉ sigil, ` · spoken`).
    /// Demo-local like notes — the protocol has no voice surface yet.
    pub fn push_user_voice(&mut self, text: String) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.user_entries.push(self.entries.len());
        self.entries.push(TranscriptEntry::User {
            text,
            attachments: 0,
            voice: true,
            from_main: false,
        });
    }

    /// Append a PARENT-AUTHORED user row (S3): a `UserMessage` that
    /// reached a chip transcript through the parent session's
    /// agent-scoped stream. The daemon's child-prompt projection is the
    /// only writer of such envelopes, so the marking is stream truth —
    /// see [`crate::session::chip_apply`].
    pub fn push_user_from_main(&mut self, text: String, attachments: usize) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.user_entries.push(self.entries.len());
        self.entries.push(TranscriptEntry::User {
            text,
            attachments,
            voice: false,
            from_main: true,
        });
    }

    /// Append a shell-builtin row (sim ShellRow — deliberately
    /// envelope-free; the sim bypasses the model/harness entirely).
    /// Apply one SEEDED transcript row (sim `U`/`A`/`T`/`N`, tui.js:469-472).
    /// Attaching a sample session replays these; it starts no turn, so the
    /// rows arrive already complete and no run state moves.
    pub fn apply_seed_row(&mut self, row: &crate::mock::SeedRow) {
        use crate::mock::SeedRow;
        self.seed_seq += 1;
        let id = crate::mock::seed_item_id(self.seed_seq);
        match row {
            SeedRow::User(text) => self.apply(&EventPayload::UserMessage {
                text: (*text).to_owned(),
                attachments: vec![],
                mode: haider_protocol::DeliveryMode::Steer,
            }),
            SeedRow::Agent(text) => self.apply(&EventPayload::Item(ItemEvent::Completed {
                item_id: id,
                item: TurnItem::AgentMessage {
                    text: (*text).into(),
                },
            })),
            SeedRow::Tool { name, desc, meta } => {
                self.apply(&EventPayload::Item(ItemEvent::Completed {
                    item_id: id.clone(),
                    item: TurnItem::ToolCall {
                        call_id: id.as_str().to_owned(),
                        name: (*name).to_owned(),
                        args: serde_json::json!({ "desc": desc, "meta": meta }),
                        status: haider_protocol::item::ToolStatus::Completed,
                    },
                }));
            }
            SeedRow::Note(text) => self.push_note((*text).to_owned()),
        }
    }

    pub fn push_shell(&mut self, cmd: String, out: String) {
        self.render_revision = self.render_revision.wrapping_add(1);
        self.entries.push(TranscriptEntry::Shell { cmd, out });
    }

    /// Toggle the voice-turn tag for blocks started from now on.
    pub fn set_voice_live(&mut self, on: bool) {
        self.voice_live = on;
    }

    /// True while a voice turn is live (spoken agent rows streaming).
    #[must_use]
    pub const fn voice_live(&self) -> bool {
        self.voice_live
    }

    /// The last turn ended in `Errored` — a TERMINAL state, distinct from
    /// every "still going" badge. Launcher rows read this so a dead turn
    /// is never dressed as a running one (owner report, W5f-0).
    #[must_use]
    pub const fn run_errored(&self) -> bool {
        matches!(self.run, Some(RunState::Errored))
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
            // M4: the plain/greppable equivalent of the warn-toned retry line.
            RunState::Retrying {
                attempt,
                max,
                delay_ms,
                ..
            } => format!(
                "✻ API error · Retrying in {}s · attempt {attempt}/{max}",
                delay_ms.div_ceil(1_000)
            ),
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

    /// The status badge's machine-readable state and human detail. This is
    /// derived directly from typed run state, never by parsing [`Self::badge`]
    /// after it has become display text.
    #[must_use]
    pub fn status_state_detail(&self) -> (String, Option<String>) {
        if matches!(self.harness, Some(HarnessStatus::Starting { .. })) {
            return ("starting".to_owned(), None);
        }
        match &self.run {
            None | Some(RunState::Done | RunState::Cancelled) => (
                "idle".to_owned(),
                self.interrupted.then(|| "interrupted".to_owned()),
            ),
            Some(RunState::Queued) => ("waiting".to_owned(), Some("queued".to_owned())),
            Some(RunState::Thinking) => ("running".to_owned(), Some("thinking".to_owned())),
            Some(RunState::Streaming) => ("running".to_owned(), Some("streaming".to_owned())),
            Some(RunState::RunningTool) => ("running".to_owned(), Some("running tool".to_owned())),
            Some(RunState::Waiting { reason }) => {
                ("waiting".to_owned(), Some(wait_reason_label(reason)))
            }
            Some(RunState::Retrying {
                attempt,
                max,
                delay_ms,
                ..
            }) => (
                "waiting".to_owned(),
                Some(format!(
                    "retrying in {}s · attempt {attempt}/{max}",
                    delay_ms.div_ceil(1_000)
                )),
            ),
            Some(RunState::InputRequired { .. }) => {
                ("waiting".to_owned(), Some("input required".to_owned()))
            }
            Some(RunState::PermissionRequired { .. }) => {
                ("waiting".to_owned(), Some("permission required".to_owned()))
            }
            Some(RunState::Compacting) => ("running".to_owned(), Some("compacting".to_owned())),
            Some(RunState::Verifying { step }) => (
                "running".to_owned(),
                Some(format!("verifying {}", verify_step_label(*step))),
            ),
            Some(RunState::Concluding) => ("running".to_owned(), Some("concluding".to_owned())),
            Some(RunState::EffectOutcomeUnknown) => (
                "errored".to_owned(),
                Some("effect outcome unknown".to_owned()),
            ),
            Some(RunState::Cancelling) => ("running".to_owned(), Some("cancelling".to_owned())),
            Some(RunState::Errored) => ("errored".to_owned(), None),
        }
    }

    /// Whether the typed run state is the durable local-subagent wait that
    /// the application augments with its live child count.
    #[must_use]
    pub fn waiting_on_local_subagent(&self) -> bool {
        matches!(
            self.run,
            Some(RunState::Waiting {
                reason: WaitReason::LocalChild
            })
        )
    }

    /// Terminal rest, idle(i) INCLUDED: no run, or the last run reached
    /// Done/Cancelled/Errored. The interrupt marker is HISTORY, not
    /// activity (owner report: a visited-then-left session wore
    /// `running…` on the launcher forever because `⏸ IDLE (i)` failed a
    /// string comparison against plain `IDLE`).
    #[must_use]
    pub fn settled(&self) -> bool {
        !matches!(self.harness, Some(HarnessStatus::Starting { .. }))
            && matches!(
                &self.run,
                None | Some(RunState::Done | RunState::Cancelled | RunState::Errored)
            )
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
            Some(RunState::Queued | RunState::Waiting { .. } | RunState::Retrying { .. }) => {
                BadgeTone::Restful
            }
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
    /// W7b: context extensions are CONSUMED, never transcript rows. A
    /// footprint snapshot feeds the meter and /tokens; the compaction
    /// intent marker becomes the pre-announce note (sim tui.js:3922
    /// vocabulary family). Returns true when the event was swallowed.
    fn consume_context_extension(&mut self, event: &ItemEvent) -> bool {
        let (item_id, item, completed) = match event {
            ItemEvent::Started { item_id, item } => (item_id, item, false),
            ItemEvent::Completed { item_id, item } => (item_id, item, true),
            ItemEvent::Delta { .. } => return false,
        };
        let TurnItem::Extension { kind, .. } = item else {
            return false;
        };
        let footprint = haider_protocol::context::ContextFootprint::from_extension_item(item);
        let intent = kind == haider_protocol::history::COMPACTION_INTENT_EXTENSION_KIND;
        let cache_transition =
            haider_protocol::cache::CacheEpochTransitionV1::from_extension_item(item);
        if footprint.is_none() && !intent && cache_transition.is_none() {
            return false;
        }
        if !completed {
            // The Started half of the marker batch: swallow without
            // opening a streaming block.
            return true;
        }
        if !self.finished_items.insert(item_id.clone()) {
            self.duplicate_items += 1;
            return true;
        }
        if let Some(footprint) = footprint {
            self.latest_footprint = Some(footprint);
            return true;
        }
        if let Some(transition) = cache_transition {
            self.push_note(transition.display_label());
            return true;
        }
        // Pre-announce (research §Q2: the sim's `· context at 85% —
        // compacting` line): percent from the latest snapshot when the
        // window is known; the honest count-free line otherwise.
        let note = self
            .latest_footprint
            .as_ref()
            .and_then(|footprint| {
                let window = footprint.context_window?;
                (window > 0).then(|| {
                    format!(
                        "· context at {}% — compacting · planned cache epoch transition; next turn history cold (summary retained · originals stay in /tree)",
                        footprint.used_tokens.saturating_mul(100) / window
                    )
                })
            })
            .unwrap_or_else(|| {
                "· compacting · planned cache epoch transition; next turn history cold — summary retained · originals stay in /tree".to_owned()
            });
        self.push_note(note);
        true
    }

    /// Latest context-occupancy snapshot (W7b) — the meter and /tokens
    /// truth source in live mode.
    #[must_use]
    pub fn latest_footprint(&self) -> Option<&haider_protocol::context::ContextFootprint> {
        self.latest_footprint.as_ref()
    }

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

    /// CU-2 safety signal: is the model mid-flight on a `computer` action
    /// that MOVES the screen (any click/type/key/move/scroll/drag), as
    /// opposed to a passive screenshot/cursor read? Drives the sacred
    /// "controlling your screen" banner so the owner can never miss that a
    /// session is driving their real machine.
    #[must_use]
    pub fn screen_control_active(&self) -> bool {
        !self.screen_control_items.is_empty()
    }

    /// View-cache invalidation token; no semantic state is derived from it.
    #[must_use]
    pub const fn render_revision(&self) -> u64 {
        self.render_revision
    }

    /// Existing-entry mutation epoch used by the bounded render cache.
    #[must_use]
    pub(crate) const fn entry_mutation_revision(&self) -> u64 {
        self.entry_mutation_revision
    }

    /// Raw prompt-entry ordinals, in transcript order. Rendering uses this
    /// compact ingest-time index for O(log U) sticky-origin lookup (`U` is
    /// the number of user prompts), never an O(N) frame-path scan.
    #[must_use]
    pub(crate) fn user_entries(&self) -> &[usize] {
        &self.user_entries
    }

    /// User prompt rows — the launcher row's turn count (sim tui.js:3248:
    /// `entries.filter((e) => e.kind === "user").length`).
    #[must_use]
    pub fn user_row_count(&self) -> u32 {
        u32::try_from(self.user_entries.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn open_menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    /// The active computer OS-permission grant card, if any.
    #[must_use]
    pub fn permission_card(&self) -> Option<&haider_protocol::permission::PermissionGrantNeeded> {
        self.permission_card.as_ref()
    }

    /// Records a `permission_grant_needed` card (additive event, decoded by
    /// [`crate::session::route_permission_event`]).
    pub fn set_permission_card(
        &mut self,
        card: haider_protocol::permission::PermissionGrantNeeded,
    ) {
        self.permission_card = Some(card);
    }

    /// Clears the card once its matching `permission_grant_resolved` arrives
    /// (request_id-scoped so a superseding card is never dropped by a stale
    /// resolution).
    pub fn resolve_permission_card(&mut self, request_id: &str) {
        if self
            .permission_card
            .as_ref()
            .is_some_and(|card| card.request_id == request_id)
        {
            self.permission_card = None;
        }
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

fn bounded_tool_reason(result: &haider_protocol::tool::BoundedResult) -> Option<String> {
    if result.status.is_completed() {
        return result.reason.as_deref().map(|reason| {
            reason
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(240)
                .collect()
        });
    }
    if let Some(presentation) = &result.presentation {
        return Some(format_error_presentation(presentation));
    }
    let reason = result.reason.as_deref().unwrap_or("tool did not complete");
    let normalized = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(normalized.chars().take(240).collect())
}

fn tool_result_carries_effect_error(
    result: &haider_protocol::tool::BoundedResult,
    error: &str,
) -> bool {
    let normalized = bounded_effect_error(error);
    result
        .reason
        .as_deref()
        .is_some_and(|reason| bounded_effect_error(reason) == normalized)
        || result.preview.contains(error)
        || result.presentation.as_ref().is_some_and(|presentation| {
            presentation.title.contains(error) || presentation.detail.contains(error)
        })
}

fn bounded_effect_error(error: &str) -> String {
    error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn tool_result_status_carries_effect_failure(
    status: haider_protocol::tool::ToolResultStatus,
) -> bool {
    matches!(
        status,
        haider_protocol::tool::ToolResultStatus::Rejected
            | haider_protocol::tool::ToolResultStatus::Conflict
            | haider_protocol::tool::ToolResultStatus::Failed
    )
}

fn tool_status_carries_effect_failure(status: haider_protocol::item::ToolStatus) -> bool {
    matches!(
        status,
        haider_protocol::item::ToolStatus::Rejected
            | haider_protocol::item::ToolStatus::Conflict
            | haider_protocol::item::ToolStatus::Failed
    )
}

/// One typed action's lowercase display word — shared by the transcript
/// string, the styled fact line, and plain mode (one vocabulary).
#[must_use]
pub const fn error_action_word(action: ErrorAction) -> &'static str {
    match action {
        ErrorAction::Retry => "retry",
        ErrorAction::Relogin => "re-login",
        ErrorAction::Reimport => "re-import",
        ErrorAction::EditKey => "edit key",
        ErrorAction::SwitchAccount => "switch account",
        ErrorAction::TopUp => "top up",
        ErrorAction::Wait => "wait",
        ErrorAction::ChooseModel => "choose model",
        ErrorAction::ContactAdmin => "contact admin",
        ErrorAction::ContinuePartial => "continue partial",
        ErrorAction::RetryFresh => "retry fresh",
        ErrorAction::None => "none",
    }
}

/// Fact-line shed ranks. Rank zero is permanent; higher ranks shed first.
pub const FACT_RANK_SUBCODE: u8 = 0;
pub const FACT_RANK_RESET: u8 = 1;
pub const FACT_RANK_ACTIONS: u8 = 2;
pub const FACT_RANK_HTTP: u8 = 3;
pub const FACT_RANK_REQUEST: u8 = 4;

/// The compact fact line's segments, display-ordered (`subcode · HTTP 429
/// · req 8f3a2c1d… · resets in 2m 14s`), each with its shed rank. A
/// missing datum DROPS its whole segment — never a placeholder. The
/// request id is shortened to its first 8 chars (the journal keeps the
/// full id; the transcript string renders it whole). The reset segment is
/// LIVE when the caller supplies the daemon clock (`reset_at_ms − now`)
/// and otherwise the static provider delay recorded at failure time.
#[must_use]
pub fn error_fact_segments(
    presentation: &ErrorPresentation,
    now_ms: Option<u64>,
) -> Vec<(String, u8)> {
    build_error_fact_segments(presentation, now_ms, 0)
}

fn build_error_fact_segments(
    presentation: &ErrorPresentation,
    now_ms: Option<u64>,
    additional_capacity: usize,
) -> Vec<(String, u8)> {
    let reset = match (
        now_ms,
        presentation.reset_at_ms,
        presentation.retry_after_ms,
    ) {
        (Some(now), Some(reset_at), _) => {
            Some(crate::format::fmt_reset_in(reset_at.saturating_sub(now)))
        }
        (_, _, Some(retry_after)) => Some(crate::format::fmt_reset_in(retry_after)),
        _ => None,
    };
    let capacity = 1
        + usize::from(presentation.provider_http_status.is_some())
        + usize::from(presentation.provider_request_id.is_some())
        + usize::from(reset.is_some())
        + additional_capacity;
    let mut segments = Vec::with_capacity(capacity);
    segments.push((presentation.subcode.as_str().to_owned(), FACT_RANK_SUBCODE));
    if let Some(status) = presentation.provider_http_status {
        segments.push((format!("HTTP {status}"), FACT_RANK_HTTP));
    }
    if let Some(request_id) = &presentation.provider_request_id {
        segments.push((
            format!("req {}", short_request_id(request_id)),
            FACT_RANK_REQUEST,
        ));
    }
    if let Some(reset) = reset {
        segments.push((reset, FACT_RANK_RESET));
    }
    segments
}

/// [`error_fact_segments`] plus the trailing `actions: …` hint — the
/// transcript row's form, where the card's option list is not on screen
/// to carry the recovery vocabulary.
#[must_use]
pub fn error_fact_segments_with_actions(
    presentation: &ErrorPresentation,
    now_ms: Option<u64>,
) -> Vec<(String, u8)> {
    let mut segments = build_error_fact_segments(presentation, now_ms, 1);
    let mut actions = "actions: ".to_owned();
    push_error_actions(&mut actions, &presentation.allowed_actions);
    segments.push((actions, FACT_RANK_ACTIONS));
    segments
}

pub(crate) fn join_error_fact_segments(segments: &[(String, u8)]) -> String {
    let capacity = segments
        .iter()
        .map(|(segment, _)| segment.len())
        .sum::<usize>()
        + segments.len().saturating_sub(1) * " · ".len();
    let mut joined = String::with_capacity(capacity);
    for (index, (segment, _)) in segments.iter().enumerate() {
        if index > 0 {
            joined.push_str(" · ");
        }
        joined.push_str(segment);
    }
    joined
}

/// E8 visual pass: the human label for a bounded in-flight retry marker
/// (`TurnItem::Extension`), shared by the styled and plain renderers so
/// both surfaces speak one sentence. `Some` marks the kind as a QUIET
/// retry fact (dim ⟳ row — recovery in progress, never alarming);
/// unknown kinds return `None` and keep the generic `⋯` treatment.
#[must_use]
pub(crate) fn retry_marker_label(kind: &str, data: &serde_json::Value) -> Option<String> {
    let label = data.get("label").and_then(serde_json::Value::as_str);
    match kind {
        // The daemon's fallback marker carries its own sentence
        // ("provider hosted web tool rejected — using local web_fetch");
        // a one-time capability switch, not an attempt counter.
        "provider_tool_fallback" => Some(
            label
                .unwrap_or("provider tool rejected — using the local fallback")
                .to_owned(),
        ),
        "tool_json_repair" => {
            if let Some(label) = label {
                return Some(label.to_owned());
            }
            let tool = data
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let mut composed = format!("malformed {tool} arguments — model asked to reissue");
            if let (Some(attempt), Some(max)) = (
                data.get("attempt").and_then(serde_json::Value::as_u64),
                data.get("max_attempts").and_then(serde_json::Value::as_u64),
            ) {
                composed.push_str(&format!(" (attempt {attempt}/{max})"));
            }
            Some(composed)
        }
        _ => None,
    }
}

/// Decode and format the durable image-created fact shared by styled and
/// plain transcript renderers. Keeping this at the projection boundary makes
/// the extension payload, rather than a renderer-specific side channel, the
/// complete UI contract.
#[must_use]
pub(crate) fn image_created_fact(
    kind: &str,
    data: &serde_json::Value,
) -> Option<(haider_protocol::image::ImageCreatedV1, String)> {
    if kind != haider_protocol::image::IMAGE_CREATED_EXTENSION_KIND {
        return None;
    }
    let image =
        serde_json::from_value::<haider_protocol::image::ImageCreatedV1>(data.clone()).ok()?;
    let dimensions = match (image.width, image.height) {
        (Some(width), Some(height)) => format!(" · {width}×{height}"),
        _ => String::new(),
    };
    let kilobytes = image.byte_len.div_ceil(1024);
    let label = format!(
        "🖼 image · {}{dimensions} · {kilobytes} KB",
        image.display_path
    );
    Some((image, label))
}

/// The fact line's request-id form: the first 8 chars, `…`-marked when
/// shortened. Support-grade full ids stay in the transcript string and
/// the journal.
fn short_request_id(request_id: &str) -> String {
    let mut characters = request_id.chars();
    let mut short: String = characters.by_ref().take(8).collect();
    if characters.next().is_some() {
        short.push('…');
    }
    short
}

/// The canonical flattened formatter for typed failures and the
/// plain/greppable authority. Shape: `{title} — {detail} [{subcode}] · HTTP {status} · req {id}
/// · {resets in …} · actions: {…}` — provider facts additive after the
/// subcode (full request id here; the styled fact line shortens it), the
/// reset human-readable via the h/m/s vocabulary, absent facts dropping
/// their whole segment.
#[must_use]
pub fn format_error_presentation(presentation: &ErrorPresentation) -> String {
    let mut out = String::with_capacity(
        presentation.title.len()
            + presentation.detail.len()
            + presentation.subcode.as_str().len()
            + 32,
    );
    // Writing to a String is infallible; discard the Ok(()) result.
    let _ = write!(
        out,
        "{} — {} [{}]",
        presentation.title,
        presentation.detail,
        presentation.subcode.as_str()
    );
    if let Some(status) = presentation.provider_http_status {
        let _ = write!(out, " · HTTP {status}");
    }
    if let Some(request_id) = &presentation.provider_request_id {
        let _ = write!(out, " · req {request_id}");
    }
    if let Some(retry_after) = presentation.retry_after_ms {
        out.push_str(" · ");
        out.push_str(&crate::format::fmt_reset_in(retry_after));
    }
    out.push_str(" · actions: ");
    push_error_actions(&mut out, &presentation.allowed_actions);
    out
}

fn push_error_actions(out: &mut String, actions: &[ErrorAction]) {
    for (index, action) in actions.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(error_action_word(*action));
    }
}

/// The sim's badge PULSE set, verbatim (tui.js:5558-5563:
/// `["WAITING", "STARTING", "PERMISSION", "EFFECT_UNKNOWN"]`) — and
/// nothing else: `IDLE_I` and `INPUT_REQUIRED` are outlined but
/// deliberately still. Keyed on the rendered label so the derived
/// `◔ WAITING · N subagents` badge pulses exactly like a run-state
/// WAITING (one vocabulary, tui.js:2815).
#[must_use]
pub fn badge_pulses(label: &str) -> bool {
    label.starts_with("◔ WAITING")
        || label.starts_with("◌ STARTING")
        || label.starts_with("? PERMISSION_REQUIRED")
        || label.starts_with("⌁ EFFECT_UNKNOWN")
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
        WaitReason::NetworkUnavailable => "network unavailable".to_owned(),
        WaitReason::ProviderBackoff => "provider backoff".to_owned(),
        WaitReason::RateLimit => "rate limit".to_owned(),
        WaitReason::RemoteChild => "unsupported remote wait — local-only".to_owned(),
        WaitReason::LocalChild => "subagent".to_owned(),
        WaitReason::DeviceUnreachable => "unsupported device wait — local-only".to_owned(),
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

/// The first 8 hex of a graph template digest — enough to read in scrollback,
/// short enough for a note row.
fn graph_digest_short(digest: &str) -> &str {
    &digest[..digest.len().min(8)]
}

/// A bounded, single-line fragment of graph evidence/abandon detail for a note
/// row: newlines collapse to spaces and the tail is elided past 80 chars.
fn graph_detail_fragment(detail: &str) -> String {
    let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in flat.chars() {
        if out.chars().count() >= 80 {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    if out.is_empty() {
        "(no detail)".to_owned()
    } else {
        out
    }
}

fn graph_block_reason(reason: haider_protocol::graph::GraphBlockReason) -> &'static str {
    use haider_protocol::graph::GraphBlockReason;
    match reason {
        GraphBlockReason::RoundsExhausted => "attempts exhausted",
        GraphBlockReason::NoProgress => "no progress (repeated failure)",
        GraphBlockReason::HumanHold => "held for review",
    }
}
