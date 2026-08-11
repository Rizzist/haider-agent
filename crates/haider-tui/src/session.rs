//! Per-session state (owner items 12+13a) — the sim's session objects.
//!
//! The sim keeps EVERY session fully materialized in one array and renders
//! the attached one (`activeId`); lifecycle state is per-session
//! (tui.js:633 `runStates`, 782-784: "a turn running in one session never
//! bleeds into another or into the main menu"). This module is that
//! array's element: seeds and user sessions alike own their transcript,
//! todos (inside the projection), chips, queue, tokens and run state.
//!
//! CHECKOUT MODEL: the ACTIVE session's state lives in `AppModel`'s
//! existing live fields (`projection`, `chips`, `turn_active`, …) exactly
//! as before this round — attach swaps a session's state INTO those fields
//! and detach swaps it back (`AppModel::checkout` / `checkin`). While a
//! session is checked out its slot here holds the neutral leftovers and
//! nothing reads it: the launcher (the only reader of the vec) renders
//! only while NO session is attached, and event routing sends the active
//! session's events through the model path. Background sessions receive
//! their events through [`SessionState::absorb_raw`].
//!
//! IDENTITY (W3c3, report R11 cut 1): a session is keyed by the protocol's
//! opaque [`SessionId`] and carries a separate local [`UiGeneration`] — see
//! [`crate::identity`] for why the two must never be the same value.

use crate::app::ChipModel;
use crate::identity::UiGeneration;
use crate::projection::{Admission, MenuScopeOwner, RawOutcome, SessionProjection};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{AgentId, SessionId};

/// One session, fully owned — the sim's `sessions[i]` (tui.js:497-579
/// seeds, 1617-1650 `newSession`). The single-branch port folds the sim's
/// active-branch fields (`tokens`, `chips`, `entries`, `todos`) into the
/// session itself.
#[derive(Debug)]
pub struct SessionState {
    /// The PROTOCOL's opaque session identity (sim `s.id`) — the daemon's
    /// string in live mode, `demo-session-{n}` in the demo. Never parsed.
    pub id: SessionId,
    /// This row's local generation: monotonic, never reused, and the key
    /// the demo driver's arms/meters and the answer outbox's origin use
    /// (report R11 cut 1 — a session id is not a stale-timer epoch).
    pub ui_gen: UiGeneration,
    /// Row name (sim `s.name` — kebab of the first message's words).
    pub name: Option<String>,
    /// The blurb (sim `s.blurb`, auto-set by the 1.5 s micro-call).
    pub title: Option<String>,
    /// Head agent `(callsign, honorific)` (sim `s.head`).
    pub head: (String, String),
    /// The head's roster index — the persistence layer's counter-restore
    /// guard reads it (sim load, tui.js:711-721). `None` for heads that
    /// never came from the roster.
    pub head_ros: Option<u64>,
    /// Working dir (sim `s.dir`).
    pub dir: String,
    pub projection: SessionProjection,
    /// Durable journal prompts for this session, newest first. This is
    /// distinct from the composer's transient submitted-input ring.
    pub prompt_history: std::collections::VecDeque<String>,
    pub cache_usage: crate::cache_usage::SessionUsageFold,
    pub chips: Vec<ChipModel>,
    /// This session's branch registry, active-branch selection, warm parked
    /// views and fork-coordinate tracker (B2b m1). The `projection`/`chips`
    /// fields above always hold the ACTIVE branch's surfaces — plus the
    /// session-global replay cursor, which switching transplants rather
    /// than duplicates (one cursor, research risk 3).
    pub branch_state: crate::branch::BranchState,
    /// This session's journaled hook facts + decision-chip state (H4) —
    /// checked in/out with the session exactly like [`Self::branch_state`].
    pub hook_facts: crate::hooks::HookFactsLog,
    /// W-A: the session-scoped background task rows (journal projection) —
    /// checked in/out whole with the session; never split per branch.
    pub tasks: crate::taskrows::TaskPanel,
    pub msg_queue: Vec<String>,
    pub queue_mode: bool,
    pub subturn_mode: bool,
    /// This session's turn engine is mid-turn (the per-session slice of
    /// the sim's `runStates` that the projection's badge does not carry).
    pub turn_active: bool,
    pub auto_resuming: bool,
    pub subtree_collapsed: bool,
    pub todos_collapsed: bool,
    // ---- Launcher-row statics (sim session fields the demo never edits).
    pub model_short: String,
    pub device: String,
    pub ago: String,
    /// The seed's advertised branch count (main included) — a display
    /// static exactly like [`Self::turns_offset`]. The LIVE branch count is
    /// DERIVED by [`Self::branches`] from the registry, never stored (B2b
    /// brief item 1: no scalar copy).
    pub branches_offset: u32,
    /// The seed's advertised turn count minus its seed transcript's user
    /// rows, so a row displays `offset + live user rows` and seeds keep
    /// their sim metas while real turns still move the number.
    pub turns_offset: u32,
    /// Roster hydration from a `session.list` summary (the additive daemon
    /// fields): real turns/tokens on the row WITHOUT attaching. `None`
    /// until a summary carrying at least one counted field arrives — an
    /// older daemon never sets it and the row degrades to its
    /// projection-derived display (never a fabricated count).
    pub summary_counts: Option<SummaryCounts>,
}

/// The additive `session.list` counts one roster row holds (launcher fix 2
/// — "0 turns · 0 tok until open+back"). `head_seq` is the freshness key
/// against the projection's applied cursor: a projection that has applied
/// AT LEAST this far holds live truth and outranks the summary
/// (checkout/checkin values beat stale summaries); a summary strictly
/// ahead of everything applied wins the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCounts {
    /// The committed head the summary was taken at.
    pub head_seq: u64,
    /// Committed user-turn count, when the daemon sent one.
    pub turns: Option<u64>,
    /// `used_tokens` of the latest durable context snapshot, when the
    /// daemon sent one (`SessionSummary::footprint_tokens`).
    pub footprint_tokens: Option<u64>,
    /// The exact/estimated honesty flag paired with `footprint_tokens`
    /// (`SessionSummary::footprint_truth`) — present exactly when the
    /// token figure is.
    pub footprint_truth: Option<haider_protocol::context::ContextFootprintTruth>,
}

impl SessionState {
    /// A neutral scratch slot — what the model's live fields hold when no
    /// session is attached, and what a checked-out slot holds meanwhile.
    #[must_use]
    pub fn neutral(id: SessionId, ui_gen: UiGeneration) -> Self {
        Self {
            id,
            ui_gen,
            name: None,
            title: None,
            head: (String::new(), String::new()),
            head_ros: None,
            dir: String::new(),
            projection: SessionProjection::new(),
            prompt_history: std::collections::VecDeque::new(),
            cache_usage: crate::cache_usage::SessionUsageFold::default(),
            chips: Vec::new(),
            branch_state: crate::branch::BranchState::default(),
            hook_facts: crate::hooks::HookFactsLog::default(),
            tasks: crate::taskrows::TaskPanel::default(),
            msg_queue: Vec::new(),
            queue_mode: false,
            subturn_mode: false,
            turn_active: false,
            auto_resuming: false,
            subtree_collapsed: false,
            todos_collapsed: false,
            model_short: String::new(),
            device: String::new(),
            ago: String::new(),
            branches_offset: 1,
            turns_offset: 0,
            summary_counts: None,
        }
    }

    /// True when the stored summary is FRESHER than everything this row's
    /// projection has applied — the only state in which summary counts may
    /// outrank checkout/checkin truth. An attach replays through the
    /// listed head, so the moment a session has been opened (or a
    /// background attachment has caught up) the projection's cursor
    /// reaches `head_seq` and the live values win again.
    fn summary_is_fresher(&self) -> bool {
        self.summary_counts.as_ref().is_some_and(|counts| {
            self.projection
                .last_applied()
                .is_none_or(|applied| counts.head_seq > applied)
        })
    }

    /// Live subagents in this session's tree (sim `sessionLive`,
    /// tui.js:325-329) — EVERY branch's, not only the displayed one: the
    /// launcher aggregate counts all branches (B2b law), so an
    /// inactive-branch child keeps the row hot without leaking its chip
    /// into another branch's view.
    #[must_use]
    pub fn live(&self) -> usize {
        crate::app::tree_live_count(&self.chips) + self.branch_state.parked_live()
    }

    /// Branch count for the launcher row: the seed static (main included)
    /// plus every DAEMON-installed named branch — derived, never a stored
    /// copy of live branch state (B2b brief item 1).
    #[must_use]
    pub fn branches(&self) -> u32 {
        self.branches_offset
            .saturating_add(u32::try_from(self.branch_state.named_count()).unwrap_or(u32::MAX))
    }

    /// Sim `sessionBusy` (tui.js:789-792): live chips OR a non-terminal
    /// run state.
    ///
    /// TWO sim beats are carved out as TERMINAL (both owner reports):
    /// `✗ ERRORED` (W5f-0 — the sim held it 1.8s; permanent here) and
    /// `⏸ IDLE (i)` (the interrupt marker is history, not activity — the
    /// old badge-string comparison against plain `IDLE` dressed every
    /// visited-then-interrupted session as `running…` forever).
    #[must_use]
    pub fn busy(&self) -> bool {
        self.live() > 0 || self.turn_active || !self.projection.settled()
    }

    /// The row's honest third state: the last turn DIED and nothing has
    /// started since. Live chips or a new turn outrank it.
    #[must_use]
    pub fn errored(&self) -> bool {
        self.live() == 0 && !self.turn_active && self.projection.run_errored()
    }

    /// Turns shown on the launcher row (sim: user entries of the active
    /// branch, tui.js:3248). A FRESHER `session.list` summary that carried
    /// a turn count wins the figure (real turns at boot, no attach);
    /// otherwise — no summary, an older daemon's summary without the
    /// field, or a projection that has applied at least as far — the
    /// projection-derived count exactly as before the additive fields.
    #[must_use]
    pub fn turns(&self) -> u32 {
        if self.summary_is_fresher()
            && let Some(turns) = self.summary_counts.as_ref().and_then(|counts| counts.turns)
        {
            return u32::try_from(turns).unwrap_or(u32::MAX);
        }
        self.turns_offset + self.projection.user_row_count()
    }

    /// The launcher row's token figure and its honesty marker:
    /// `(tokens, estimated)`. A FRESHER summary's footprint wins —
    /// `footprint_tokens`, flagged when the paired `footprint_truth`
    /// marked the figure Estimated (the renderer's `~` prefix); otherwise
    /// the projection's cumulative context tokens, exactly as before the
    /// additive fields.
    #[must_use]
    pub fn row_tokens(&self) -> (u64, bool) {
        if self.summary_is_fresher()
            && let Some(counts) = self.summary_counts.as_ref()
            && let Some(tokens) = counts.footprint_tokens
        {
            return (
                tokens,
                matches!(
                    counts.footprint_truth,
                    Some(haider_protocol::context::ContextFootprintTruth::Estimated)
                ),
            );
        }
        (self.projection.context_tokens(), false)
    }

    /// This row's token truth or NOTHING — the S4 chip-row join's source.
    /// Unlike [`Self::row_tokens`] (a launcher display that may honestly
    /// show `0 tok` for a row it is looking at), a joined figure on
    /// another surface must not collapse "unknown" into zero, so:
    ///
    /// * a FRESHER summary's `footprint_tokens` wins (`Some(0)` is real —
    ///   the daemon reports it exclusively for truly empty sessions);
    /// * else the projection's cumulative usage when any has applied;
    /// * else a STALE summary's footprint (behind live truth but still
    ///   this session's own journal — better than silence);
    /// * else `None` — the caller drops the segment.
    #[must_use]
    pub fn known_tokens(&self) -> Option<u64> {
        let summary_tokens = self
            .summary_counts
            .as_ref()
            .and_then(|counts| counts.footprint_tokens);
        if self.summary_is_fresher()
            && let Some(tokens) = summary_tokens
        {
            return Some(tokens);
        }
        let live = self.projection.context_tokens();
        if live > 0 {
            return Some(live);
        }
        summary_tokens
    }

    /// Route one RAW envelope into this (non-attached) session — the
    /// background half of the W3c3 router (report R11 cut 2).
    ///
    /// Validates the frame's session id, runs the STRICT cursor gate, and
    /// only then reduces. A gap stops reduction with the cursor unmoved so
    /// the caller can reattach after the last fully applied sequence BEFORE
    /// any later envelope mutates state.
    ///
    /// B2b: the branch command-state hooks (fork-coordinate tracker, branch
    /// registry) run for `Apply` AND `Skip` — command coordinates are not
    /// display state, so `render.ui == false` records them while still
    /// painting nothing. Content then routes type-first (aggregates
    /// session-global), then branch, then agent.
    pub fn absorb_raw(&mut self, envelope: &RawEnvelope) -> RawOutcome {
        if envelope.session_id != self.id {
            return RawOutcome::WrongSession;
        }
        match self.projection.admit(envelope) {
            Admission::Duplicate => RawOutcome::Duplicate,
            Admission::Gap { after_seq } => RawOutcome::Gap { after_seq },
            admission @ (Admission::Skip | Admission::Apply) => {
                let note = self.branch_state.note_admitted(envelope);
                // H4: hook facts are recorded for Apply AND Skip in the
                // background too — the attached twin does the same, so a
                // later checkout carries the decision chip and firings.
                self.hook_facts.note_envelope(envelope);
                if let crate::branch::AdmittedNote::BranchInstalled(id) = &note {
                    // The fork this session itself issued: the receipt is
                    // already in and armed activation — the JOURNAL fact is
                    // what installs and activates (daemon truth first).
                    let id = id.clone();
                    if self.branch_state.take_pending_activation(&id) {
                        self.switch_branch(Some(&id));
                    }
                }
                if matches!(note, crate::branch::AdmittedNote::Content) {
                    match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                        Ok(payload) => {
                            if envelope.agent_id.is_none()
                                && let EventPayload::UserMessage { text, .. } = &payload
                            {
                                self.prompt_history.push_front(text.clone());
                            }
                            if admission == Admission::Apply {
                                self.route_admitted(
                                    &payload,
                                    envelope.branch_id.as_ref(),
                                    envelope.agent_id.as_ref(),
                                    envelope.committed_at_ms,
                                );
                            }
                        }
                        // S3/W-A: the additive agent- and task-event unions
                        // ride raw envelopes OUTSIDE `EventPayload` — try
                        // them before counting the payload unknown (both
                        // twins).
                        Err(_) if admission == Admission::Apply => {
                            if !route_agent_event(
                                &mut self.branch_state,
                                &mut self.projection,
                                &mut self.chips,
                                envelope,
                            ) && !route_task_event(
                                &mut self.tasks,
                                &mut self.branch_state,
                                &mut self.projection,
                                envelope,
                            ) {
                                self.projection.count_unknown_payload();
                            }
                        }
                        Err(_) => {}
                    }
                }
                RawOutcome::Applied
            }
        }
    }

    /// Route one admitted content payload: aggregate session-scope types
    /// FIRST (risk 6), then branch (outer), then agent — the background
    /// half; `AppModel::route_admitted` is the attached twin. `at_ms` is
    /// the envelope's `committed_at_ms` — the chip clocks' time base (S4).
    fn route_admitted(
        &mut self,
        payload: &EventPayload,
        branch: Option<&haider_protocol::ids::BranchId>,
        agent: Option<&AgentId>,
        at_ms: u64,
    ) {
        use crate::branch::BranchScope;
        if let EventPayload::Usage(usage) = payload {
            self.cache_usage.note(usage);
        }
        match self.branch_state.scope_of(payload, branch) {
            BranchScope::Aggregate => {
                self.branch_state.apply_aggregate_to_parked(payload);
                self.absorb_envelope(payload);
            }
            BranchScope::Active => self.absorb_scoped(payload, agent, at_ms),
            BranchScope::ParkedMain => {
                self.note_parked_turn(payload);
                if let Some(view) = self.branch_state.parked_main_mut() {
                    crate::branch::absorb_into_view(view, payload, agent, at_ms);
                }
            }
            BranchScope::ParkedNamed(id) => {
                self.note_parked_turn(payload);
                if let Some(view) = self.branch_state.view_mut(&id) {
                    crate::branch::absorb_into_view(view, payload, agent, at_ms);
                }
            }
            BranchScope::Orphan => self.branch_state.count_orphan(),
        }
    }

    /// Session-wide run bookkeeping for INACTIVE-branch content: run
    /// execution is session-wide (research Q2 keeps the sim's policy), so a
    /// turn on any branch flips this session's busy state — its DISPLAY
    /// stays in that branch's view.
    fn note_parked_turn(&mut self, payload: &EventPayload) {
        if let EventPayload::UserMessage { .. } = payload {
            self.turn_active = true;
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
            self.auto_resuming = false;
        }
    }

    /// Switch this background session's displayed branch — the slot twin of
    /// `AppModel::switch_branch` (no scroll/menu chrome to reset here).
    pub fn switch_branch(&mut self, target: Option<&haider_protocol::ids::BranchId>) -> bool {
        self.branch_state
            .switch(target, &mut self.projection, &mut self.chips)
            == crate::branch::SwitchOutcome::Switched
    }

    /// One admitted payload, routed by SCOPE (report R11 cut 2) — the
    /// BACKGROUND half. [`classify`] makes the decision so this path and
    /// the attached path in `AppModel` can never diverge.
    pub fn absorb_scoped(&mut self, payload: &EventPayload, agent: Option<&AgentId>, at_ms: u64) {
        match classify(&mut self.projection, &self.chips, payload, agent) {
            Destination::Agent => apply_agent_payload(&mut self.chips, payload, at_ms),
            Destination::Chip(target) => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &target) {
                    chip_apply(chip, payload, at_ms);
                }
            }
            Destination::Session => self.absorb_envelope(payload),
        }
    }

    /// The SESSION-scoped half of the router — mirrors the session-scoped
    /// part of `AppModel::handle_envelope` (screen flips excluded). The
    /// demo driver's background arm calls this directly with its scripted
    /// payloads (which carry no envelope, so no scope and no cursor).
    pub fn absorb_envelope(&mut self, payload: &EventPayload) {
        if let EventPayload::Usage(usage) = payload {
            self.cache_usage.note(usage);
        }
        if let EventPayload::UserMessage { .. } = payload {
            self.turn_active = true;
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
            self.auto_resuming = false;
            // The ♪ tag ends where the turn ends (review P2-10) — same law
            // as the active path.
            self.projection.set_voice_live(false);
        }
        self.projection.apply(payload);
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            broadcast(&mut self.chips, payload);
        }
    }
}

/// Where an admitted payload must land (report R11 cut 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The session's own reducer — `AppModel::handle_envelope` on the
    /// attached path, [`SessionState::absorb_envelope`] in the background.
    Session,
    /// The named agent's chip transcript.
    Chip(String),
    /// Chip-TREE bookkeeping — see [`apply_agent_payload`].
    Agent,
}

/// Decide where one admitted payload lands, recording menu ownership on the
/// way (report R11 cut 2). Shared by BOTH routes on purpose: the attached
/// session and a background session must reduce the same stream the same
/// way, and a second copy of this decision is exactly how they would drift.
///
/// * agent lifecycle → [`Destination::Agent`] (it names its own agent, so
///   the envelope's scope is irrelevant);
/// * `MenuOpened` → records the opening scope, then routes like any payload;
/// * `MenuAnswered`/`MenuClosed` → the RECORDED opener, so a subagent's
///   answer can never close the session's blocking card. With no recorded
///   opening it falls back to the session reducer, which broadcasts the
///   answer to every chip transcript exactly as it did before W3c3 — the
///   demo answers every card through that fallback (its chip cards never
///   ride an envelope), so the map is purely additive for live streams;
/// * everything else → the chip named by `agent_id` when this session knows
///   it, else the session.
pub fn classify(
    projection: &mut SessionProjection,
    chips: &[ChipModel],
    payload: &EventPayload,
    agent: Option<&AgentId>,
) -> Destination {
    match payload {
        EventPayload::AgentSpawned(_)
        | EventPayload::AgentChipState { .. }
        | EventPayload::AgentReport(_) => Destination::Agent,
        EventPayload::MenuOpened(menu) => {
            let owner = agent.map_or(MenuScopeOwner::Session, |agent| {
                MenuScopeOwner::Agent(agent.clone())
            });
            projection.note_menu_owner(menu.id.clone(), owner);
            chip_or_session(chips, agent)
        }
        EventPayload::MenuAnswered(haider_protocol::menu::MenuAnswer { menu, .. })
        | EventPayload::MenuClosed { menu, .. } => match projection.menu_owner(menu) {
            Some(MenuScopeOwner::Agent(owner)) => Destination::Chip(owner.as_str().to_owned()),
            _ => Destination::Session,
        },
        _ => chip_or_session(chips, agent),
    }
}

fn chip_or_session(chips: &[ChipModel], agent: Option<&AgentId>) -> Destination {
    match agent {
        Some(agent) if crate::app::find_chip(chips, agent.as_str()).is_some() => {
            Destination::Chip(agent.as_str().to_owned())
        }
        _ => Destination::Session,
    }
}

/// Chip-tree bookkeeping for the three agent payloads (report R11 cut 2).
///
/// `AgentSpawned` creates the chip from its manifest (idempotent under
/// replay), `AgentChipState` is the SOLE chip-state authority, and
/// `AgentReport` contributes ONLY summary/verification content — never
/// state. Shared by the attached and background routes. `at_ms` is the
/// envelope's `committed_at_ms`: the spawn instant on `AgentSpawned`, an
/// event-clock advance on the other two (S4 — replay re-derives the same
/// figures from the same journal timestamps).
pub fn apply_agent_payload(chips: &mut Vec<ChipModel>, payload: &EventPayload, at_ms: u64) {
    match payload {
        EventPayload::AgentSpawned(manifest) => {
            if crate::app::find_chip_mut(chips, manifest.agent.as_str()).is_some() {
                return;
            }
            let mut chip = ChipModel::from_manifest(manifest);
            chip.spawned_at_ms = Some(at_ms);
            chip.note_event_at(at_ms);
            if manifest.callsign.is_none() {
                // W6d (owner ask): live children claim honor-roll
                // callsigns exactly like the sim — deterministically from
                // journal order (replay/reload re-derive the same name;
                // the wire stays additive: a future daemon-minted
                // callsign simply wins).
                let index = crate::script::ROSTER_FIRST_CLAIM
                    + crate::app::flatten_chips(chips).len() as u64;
                let roster = crate::script::roster_at(index);
                chip.ros = Some(roster.ros);
                chip.callsign = roster.callsign;
                chip.hon = roster.hon;
                chip.full = roster.full;
            }
            match manifest
                .parent
                .as_ref()
                .and_then(|parent| crate::app::find_chip_mut(chips, parent.as_str()))
            {
                Some(parent) => parent.children.push(chip),
                None => chips.push(chip),
            }
        }
        EventPayload::AgentChipState { agent, chip } => {
            if let Some(model) = crate::app::find_chip_mut(chips, agent.as_str()) {
                // S4: through the shared transition law — a terminal flip
                // freezes the chip's clock at this envelope's timestamp.
                model.set_state_at(crate::script::ChipDisplayState::from_protocol(chip), at_ms);
            }
        }
        EventPayload::AgentReport(report) => {
            if let Some(model) = crate::app::find_chip_mut(chips, report.agent.as_str()) {
                model.note_event_at(at_ms);
                model.transcript.push_note(crate::app::report_note(report));
            }
        }
        _ => {}
    }
}

/// Apply one agent-scoped payload to a chip transcript — the
/// `Destination::Chip` arm all three routes share (attached, background,
/// parked view), so the decision can never drift.
///
/// S3: a `UserMessage` that reached a chip through the PARENT session's
/// stream is parent-authored by construction — the daemon's child-prompt
/// projection (the spawn prompt, `message_subagent` steers, and
/// chip-composer sends alike) is the only writer of agent-scoped user
/// rows — so the row wears the `→ · from main` marking instead of a plain
/// user row. The demo driver's `ChipEmit` beats apply straight to the
/// chip projection and stay plain: the sim fabricates locally and must
/// not dress its rows as parent-stream truth.
pub fn chip_apply(chip: &mut ChipModel, payload: &EventPayload, at_ms: u64) {
    // S4: every child-attributed envelope advances this chip's event
    // clock (the frozen-final measure's far end) before the payload lands.
    chip.note_event_at(at_ms);
    if let EventPayload::UserMessage {
        text, attachments, ..
    } = payload
    {
        chip.transcript
            .push_user_from_main(text.clone(), attachments.len());
    } else {
        chip.transcript.apply(payload);
    }
}

/// Route one admitted envelope whose payload sits OUTSIDE the
/// `EventPayload` union (S3): the additive `AgentEventPayload` kinds ride
/// raw envelopes so exhaustive consumers stay source-compatible, and this
/// hook is the raw-envelope reader `haider_protocol::agent` promises.
/// Shared by the attached and background twins. Returns `false` when the
/// payload is no agent event either — the caller counts it unknown,
/// exactly as before.
///
/// `AgentMessaged` becomes a parent-timeline note on the branch the
/// envelope names — the active surfaces, a parked view, or the orphan
/// counter. It is never guessed into another branch's transcript, and the
/// callsign comes from the OWNING branch's chips (an unknown agent keeps
/// its opaque id — no fabricated names).
pub fn route_agent_event(
    branch_state: &mut crate::branch::BranchState,
    projection: &mut SessionProjection,
    chips: &mut [ChipModel],
    envelope: &RawEnvelope,
) -> bool {
    use crate::branch::BranchScope;
    if let Some(metrics) =
        haider_protocol::agent::AgentMetricsSnapshot::from_payload_value(&envelope.payload)
    {
        let Some(agent) = metrics.agent.clone() else {
            return true;
        };
        let apply = |chips: &mut [ChipModel], metrics| {
            if let Some(chip) = crate::app::find_chip_mut(chips, agent.as_str()) {
                chip.note_metrics(metrics);
            }
        };
        match branch_state.content_scope(envelope.branch_id.as_ref()) {
            BranchScope::Active => apply(chips, metrics),
            BranchScope::ParkedMain => {
                if let Some(view) = branch_state.parked_main_mut() {
                    apply(&mut view.chips, metrics);
                }
            }
            BranchScope::ParkedNamed(id) => {
                if let Some(view) = branch_state.view_mut(&id) {
                    apply(&mut view.chips, metrics);
                }
            }
            BranchScope::Orphan => branch_state.count_orphan(),
            BranchScope::Aggregate => {}
        }
        return true;
    }
    let Some(fact) = haider_protocol::agent::AgentMessaged::from_payload_value(&envelope.payload)
    else {
        return false;
    };
    match branch_state.content_scope(envelope.branch_id.as_ref()) {
        BranchScope::Active => projection.push_note(crate::app::messaged_note(chips, &fact)),
        BranchScope::ParkedMain => {
            if let Some(view) = branch_state.parked_main_mut() {
                let note = crate::app::messaged_note(&view.chips, &fact);
                view.projection.push_note(note);
            }
        }
        BranchScope::ParkedNamed(id) => {
            if let Some(view) = branch_state.view_mut(&id) {
                let note = crate::app::messaged_note(&view.chips, &fact);
                view.projection.push_note(note);
            }
        }
        BranchScope::Orphan => branch_state.count_orphan(),
        // `content_scope` never says Aggregate (aggregate is a TYPE
        // decision, and this payload's type is agent-fact) — nothing to do.
        BranchScope::Aggregate => {}
    }
    true
}

/// W-A twin of [`route_agent_event`] for the additive task-event union:
/// the PANEL applies session-wide (tasks are session-scoped by runtime
/// law), while the ambient NOTE lands on the branch timeline the fact was
/// stamped with — active surface or a warm parked view.
pub fn route_task_event(
    tasks: &mut crate::taskrows::TaskPanel,
    branch_state: &mut crate::branch::BranchState,
    projection: &mut SessionProjection,
    envelope: &RawEnvelope,
) -> bool {
    use crate::branch::BranchScope;
    let Some(fact) = haider_protocol::task::TaskEventPayload::from_payload_value(&envelope.payload)
    else {
        return false;
    };
    tasks.apply(&fact);
    let note = crate::taskrows::task_note(&fact);
    match branch_state.content_scope(envelope.branch_id.as_ref()) {
        BranchScope::Active => projection.push_note(note),
        BranchScope::ParkedMain => {
            if let Some(view) = branch_state.parked_main_mut() {
                view.projection.push_note(note);
            }
        }
        BranchScope::ParkedNamed(id) => {
            if let Some(view) = branch_state.view_mut(&id) {
                view.projection.push_note(note);
            }
        }
        BranchScope::Orphan => branch_state.count_orphan(),
        // `content_scope` never says Aggregate for a task fact (aggregate
        // is a TYPE decision) — nothing to do.
        BranchScope::Aggregate => {}
    }
    true
}

/// Apply one payload to every chip transcript in the tree — the answer
/// fallback for menus with no recorded opening (see [`classify`]).
pub fn broadcast(chips: &mut [ChipModel], payload: &EventPayload) {
    for chip in chips {
        chip.transcript.apply(payload);
        broadcast(&mut chip.children, payload);
    }
}

/// Drop chips closed earlier whose 5 s removal never fired (sim
/// `sweepClosedChips`, tui.js:296 — run by `openSession` and the
/// persistence load).
pub fn sweep_closed_chips(chips: &mut Vec<ChipModel>) {
    chips.retain(|chip| !chip.closed);
    for chip in chips {
        sweep_closed_chips(&mut chip.children);
    }
}
