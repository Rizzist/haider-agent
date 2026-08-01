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
    pub chips: Vec<ChipModel>,
    pub msg_queue: Vec<String>,
    pub queue_mode: bool,
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
    pub branches: u32,
    /// The seed's advertised turn count minus its seed transcript's user
    /// rows, so a row displays `offset + live user rows` and seeds keep
    /// their sim metas while real turns still move the number.
    pub turns_offset: u32,
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
            chips: Vec::new(),
            msg_queue: Vec::new(),
            queue_mode: false,
            turn_active: false,
            auto_resuming: false,
            subtree_collapsed: false,
            todos_collapsed: false,
            model_short: String::new(),
            device: String::new(),
            ago: String::new(),
            branches: 1,
            turns_offset: 0,
        }
    }

    /// Live subagents in this session's tree (sim `sessionLive`,
    /// tui.js:325-329).
    #[must_use]
    pub fn live(&self) -> usize {
        crate::app::tree_live_count(&self.chips)
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
    /// branch, tui.js:3248).
    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns_offset + self.projection.user_row_count()
    }

    /// Route one RAW envelope into this (non-attached) session — the
    /// background half of the W3c3 router (report R11 cut 2).
    ///
    /// Validates the frame's session id, runs the STRICT cursor gate, and
    /// only then reduces. A gap stops reduction with the cursor unmoved so
    /// the caller can reattach after the last fully applied sequence BEFORE
    /// any later envelope mutates state.
    pub fn absorb_raw(&mut self, envelope: &RawEnvelope) -> RawOutcome {
        if envelope.session_id != self.id {
            return RawOutcome::WrongSession;
        }
        match self.projection.admit(envelope) {
            Admission::Duplicate => RawOutcome::Duplicate,
            Admission::Gap { after_seq } => RawOutcome::Gap { after_seq },
            Admission::Skip => RawOutcome::Applied,
            Admission::Apply => {
                match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                    Ok(payload) => self.absorb_scoped(&payload, envelope.agent_id.as_ref()),
                    Err(_) => self.projection.count_unknown_payload(),
                }
                RawOutcome::Applied
            }
        }
    }

    /// One admitted payload, routed by SCOPE (report R11 cut 2) — the
    /// BACKGROUND half. [`classify`] makes the decision so this path and
    /// the attached path in `AppModel` can never diverge.
    pub fn absorb_scoped(&mut self, payload: &EventPayload, agent: Option<&AgentId>) {
        match classify(&mut self.projection, &self.chips, payload, agent) {
            Destination::Agent => apply_agent_payload(&mut self.chips, payload),
            Destination::Chip(target) => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &target) {
                    chip.transcript.apply(payload);
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
/// state. Shared by the attached and background routes.
pub fn apply_agent_payload(chips: &mut Vec<ChipModel>, payload: &EventPayload) {
    match payload {
        EventPayload::AgentSpawned(manifest) => {
            if crate::app::find_chip_mut(chips, manifest.agent.as_str()).is_some() {
                return;
            }
            let chip = ChipModel::from_manifest(manifest);
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
                model.state = crate::script::ChipDisplayState::from_protocol(chip);
            }
        }
        EventPayload::AgentReport(report) => {
            if let Some(model) = crate::app::find_chip_mut(chips, report.agent.as_str()) {
                model.transcript.push_note(crate::app::report_note(report));
            }
        }
        _ => {}
    }
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
