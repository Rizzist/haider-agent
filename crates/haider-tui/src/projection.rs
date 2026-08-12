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
use haider_protocol::ids::{ItemId, NodeId};
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
    /// A turn item and its streaming accumulation state.
    Item(ItemBlock),
    /// A display-only UI note (sim `NoteRow`): auto-title, interrupt, and
    /// mid-turn echoes. The ONLY non-envelope entry source besides Shell.
    Note { text: String },
    /// A failed run's PUBLIC reason (`EventPayload::RunFailed`, W5g-6).
    /// The owner hit three silent ✗ ERRORED badges before this row
    /// existed — the reason was always in the envelope, never on screen.
    /// `presentation` keeps the TYPED contract beside the flattened text
    /// (E-wave visual pass) so the styled renderer can give it the card
    /// treatment — err title, dim detail, muted fact line — while `text`
    /// stays the plain/greppable authority; client-observed failures and
    /// non-presentation wire errors carry `None` and keep the baseline
    /// one-line render.
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
            tool_reason: None,
            spoken: false,
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

/// Session display state, reduced from the envelope stream.
#[derive(Debug, Default)]
pub struct SessionProjection {
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
    /// Item ids whose lifecycle has closed — a re-delivered `Completed` (or a
    /// stale `Started`) for one of these is idempotently ignored (replace
    /// semantics: one item, one block, ever).
    finished_items: std::collections::HashSet<ItemId>,
    /// Results may precede or follow their completed item during attach.
    pending_tool_results: std::collections::HashMap<String, haider_protocol::tool::BoundedResult>,
    menu: Option<Menu>,
    /// Menu-id → opening scope (report R11 cut 2). Stream-scoped: hydration
    /// starts it empty, and a menu with no recorded opening falls back to
    /// the pre-W3c3 broadcast.
    menu_owner: std::collections::HashMap<haider_protocol::ids::MenuId, MenuScopeOwner>,
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
        Self {
            entries,
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
        match payload {
            EventPayload::HarnessStatus(status) => self.harness = Some(status.clone()),
            EventPayload::SessionState(state) => {
                if let haider_protocol::state::SessionState::Idle { interrupted } = state {
                    self.interrupted = *interrupted;
                }
            }
            EventPayload::RunState(run) => {
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
            } => self.entries.push(TranscriptEntry::User {
                text: text.clone(),
                attachments: attachments.len(),
                voice: false,
                from_main: false,
            }),
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
                        || {
                            let code = serde_json::to_value(code)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_owned))
                                .unwrap_or_else(|| format!("{code:?}"));
                            format!("{code} — {message}")
                        },
                        format_error_presentation,
                    ),
                    presentation: presentation.clone(),
                });
            }
            // B2b-m3: a committed node ANCHORS its display entry — never a
            // transcript row of its own (the sim's tree reads entries; the
            // node is the durable identity riding beside them).
            EventPayload::NodeCommitted(node) => self.record_node_anchor(node),
            // F2e error-visibility sweep: every turn-level failure the
            // wire can carry surfaces as a VISIBLE session-view line with
            // its public reason — never a silent IDLE.
            EventPayload::Effect(haider_protocol::effect::EffectPhase::Outcome {
                outcome, ..
            }) => {
                use haider_protocol::effect::EffectOutcome;
                match outcome {
                    EffectOutcome::Failed { error } => {
                        self.entries.push(TranscriptEntry::Error {
                            text: format!("effect failed — {error}"),
                            presentation: None,
                        });
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
            // Consumed by later waves (effects timeline, subagent tree,
            // accounts). The projection stays tolerant of them now.
            EventPayload::Effect(_)
            | EventPayload::AgentSpawned(_)
            | EventPayload::AgentReport(_)
            | EventPayload::AgentChipState { .. } => {}
        }
    }

    fn apply_tool_result(&mut self, call_id: &str, result: &haider_protocol::tool::BoundedResult) {
        let reason = bounded_tool_reason(result);
        if let Some(block) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            TranscriptEntry::Item(block) => match &mut block.item {
                TurnItem::ToolCall {
                    call_id: known,
                    status,
                    ..
                } if known == call_id => {
                    *status = result.status.item_status();
                    Some(block)
                }
                _ => None,
            },
            _ => None,
        }) {
            block.tool_reason = reason;
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
        self.entries.push(TranscriptEntry::Error {
            text,
            presentation: None,
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
                        self.entries
                            .push(TranscriptEntry::Item(ItemBlock::new_spoken(
                                item_id.clone(),
                                item.clone(),
                                false,
                                self.voice_live,
                            )));
                    }
                }
                if let TurnItem::ToolCall { call_id, .. } = item
                    && let Some(result) = self.pending_tool_results.remove(call_id)
                {
                    self.apply_tool_result(call_id, &result);
                }
            }
        }
    }

    fn apply_delta(&mut self, item_id: &ItemId, delta: &ItemDelta) {
        // W-G: OUTPUT text characters that actually land on the open assistant
        // block, tallied AFTER the block borrow ends (the counter and the
        // block are both fields of `self`). Reasoning text is NOT output.
        let mut output_chars = 0u64;
        {
            let Some(block) = self.open_block_mut(item_id) else {
                self.orphan_deltas += 1;
                return;
            };
            match delta {
                ItemDelta::Text { text } => {
                    if let TurnItem::AgentMessage { text: body } = &mut block.item {
                        body.push_str(text);
                        output_chars = text.chars().count() as u64;
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

    /// True while the run is in its THINKING beat — the sim shows a
    /// transient `● thinking…` line at the transcript tail.
    #[must_use]
    pub const fn is_thinking(&self) -> bool {
        matches!(self.run, Some(RunState::Thinking))
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
        self.entries.push(TranscriptEntry::Note { text });
    }

    /// Append a voice user row (sim /say + talk: ◉ sigil, ` · spoken`).
    /// Demo-local like notes — the protocol has no voice surface yet.
    pub fn push_user_voice(&mut self, text: String) {
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
                    text: (*text).to_owned(),
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

    /// User prompt rows — the launcher row's turn count (sim tui.js:3248:
    /// `entries.filter((e) => e.kind === "user").length`).
    #[must_use]
    pub fn user_row_count(&self) -> u32 {
        u32::try_from(
            self.entries
                .iter()
                .filter(|entry| matches!(entry, TranscriptEntry::User { .. }))
                .count(),
        )
        .unwrap_or(u32::MAX)
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

fn bounded_tool_reason(result: &haider_protocol::tool::BoundedResult) -> Option<String> {
    if result.status.is_completed() {
        return None;
    }
    if let Some(presentation) = &result.presentation {
        return Some(format_error_presentation(presentation));
    }
    let reason = result.reason.as_deref().unwrap_or("tool did not complete");
    let normalized = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(normalized.chars().take(240).collect())
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

/// Fact-line SHED RANKS (E-wave visual pass): under width pressure the
/// fact line drops WHOLE segments, highest rank first, display order
/// untouched — the S4 metrics-degradation idiom. The subcode is the
/// stable identity and never sheds; the reset countdown outranks the
/// actions hint, which outranks provider metadata.
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
    let mut segments = vec![(presentation.subcode.as_str().to_owned(), FACT_RANK_SUBCODE)];
    if let Some(status) = presentation.provider_http_status {
        segments.push((format!("HTTP {status}"), FACT_RANK_HTTP));
    }
    if let Some(request_id) = &presentation.provider_request_id {
        segments.push((
            format!("req {}", short_request_id(request_id)),
            FACT_RANK_REQUEST,
        ));
    }
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
    let mut segments = error_fact_segments(presentation, now_ms);
    let actions = presentation
        .allowed_actions
        .iter()
        .map(|action| error_action_word(*action))
        .collect::<Vec<_>>()
        .join(", ");
    segments.push((format!("actions: {actions}"), FACT_RANK_ACTIONS));
    segments
}

/// The fact line's request-id form: the first 8 chars, `…`-marked when
/// shortened. Support-grade full ids stay in the transcript string and
/// the journal.
fn short_request_id(request_id: &str) -> String {
    let mut short: String = request_id.chars().take(8).collect();
    if request_id.chars().count() > 8 {
        short.push('…');
    }
    short
}

/// The single baseline text formatter for typed failures — the transcript
/// row's flattened text and the plain/greppable authority. E-wave visual
/// pass shape: `{title} — {detail} [{subcode}] · HTTP {status} · req {id}
/// · {resets in …} · actions: {…}` — provider facts additive after the
/// subcode (full request id here; the styled fact line shortens it), the
/// reset human-readable via the h/m/s vocabulary, absent facts dropping
/// their whole segment.
#[must_use]
pub fn format_error_presentation(presentation: &ErrorPresentation) -> String {
    let actions = presentation
        .allowed_actions
        .iter()
        .map(|action| error_action_word(*action))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "{} — {} [{}]",
        presentation.title,
        presentation.detail,
        presentation.subcode.as_str(),
    );
    if let Some(status) = presentation.provider_http_status {
        out.push_str(&format!(" · HTTP {status}"));
    }
    if let Some(request_id) = &presentation.provider_request_id {
        out.push_str(&format!(" · req {request_id}"));
    }
    if let Some(retry_after) = presentation.retry_after_ms {
        out.push_str(&format!(" · {}", crate::format::fmt_reset_in(retry_after)));
    }
    out.push_str(&format!(" · actions: {actions}"));
    out
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
