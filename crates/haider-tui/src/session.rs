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
//! their events through [`SessionState::absorb`].

use crate::app::ChipModel;
use crate::projection::SessionProjection;

/// One session, fully owned — the sim's `sessions[i]` (tui.js:497-579
/// seeds, 1617-1650 `newSession`). The single-branch port folds the sim's
/// active-branch fields (`tokens`, `chips`, `entries`, `todos`) into the
/// session itself.
#[derive(Debug)]
pub struct SessionState {
    /// Stable identity (sim `s.id`). Never reused; guards stale answers
    /// and auto-titles the way `session_epoch` always did.
    pub id: u64,
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
    pub fn neutral(id: u64) -> Self {
        Self {
            id,
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

    /// Sim `sessionBusy` (tui.js:789-792): live chips OR a non-idle run
    /// state. `IDLE_I` counts as busy there (only plain idle is excluded);
    /// the projection's badge speaks the same vocabulary.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.live() > 0 || self.turn_active || self.projection.badge() != "IDLE"
    }

    /// Turns shown on the launcher row (sim: user entries of the active
    /// branch, tui.js:3248).
    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns_offset + self.projection.user_row_count()
    }

    /// Apply one background event to this (non-attached) session — the
    /// state-mutating half of the driver's active-session `consume` arms,
    /// against THIS session's fields. The active path's screen flips,
    /// menu-selection resets and view-path edits are attached-surface
    /// concerns and deliberately have no counterpart here; everything that
    /// is SESSION state must stay law-identical with the active arms.
    pub fn absorb(&mut self, event: crate::script::DemoEvent) {
        use crate::script::DemoEvent;
        match event {
            DemoEvent::Envelope(payload) => self.absorb_envelope(&payload),
            DemoEvent::Note(text) => self.projection.push_note(text),
            DemoEvent::Voice(on) => self.projection.set_voice_live(on),
            DemoEvent::ChipAdd(seed) => {
                let parent = seed.parent.clone();
                let chip = ChipModel::from_seed(*seed);
                match parent.and_then(|agent| crate::app::find_chip_mut(&mut self.chips, &agent)) {
                    Some(parent_chip) => parent_chip.children.push(chip),
                    None => self.chips.push(chip),
                }
            }
            DemoEvent::ChipState { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    chip.state = state;
                }
            }
            DemoEvent::ChipEmit { agent, payload } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    chip.transcript.apply(&payload);
                }
            }
            DemoEvent::ChipNote { agent, text } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    chip.transcript.push_note(text);
                }
            }
            DemoEvent::ChipTokens { agent, n } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    chip.tokens = chip.tokens.saturating_add(n);
                }
            }
            DemoEvent::ChipQuestion {
                agent,
                recovery,
                text,
                options,
            } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    // Atomic with the state, exactly as the active arm.
                    chip.state = if recovery {
                        crate::script::ChipDisplayState::Error
                    } else {
                        crate::script::ChipDisplayState::InputRequired
                    };
                    chip.question = Some(crate::app::ChipQuestion {
                        recovery,
                        text,
                        options,
                        resolved: false,
                    });
                }
            }
            DemoEvent::ChipResolve { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    if let Some(question) = &mut chip.question {
                        question.resolved = true;
                    }
                    chip.state = state;
                }
            }
            DemoEvent::ChipQuestionClear { agent, state } => {
                if let Some(chip) = crate::app::find_chip_mut(&mut self.chips, &agent) {
                    chip.question = None;
                    chip.state = state;
                }
            }
            DemoEvent::ChipRemove { agent } => {
                let _ = crate::app::remove_chip(&mut self.chips, &agent);
            }
            // Driver-owned events (Dispatch, TurnEnd, AutoResume, Answer,
            // AutoTitle, chip close lifecycle, aura, talk) are routed by
            // `consume` itself — they spawn scripts or touch surfaces this
            // struct does not own.
            _ => {}
        }
    }

    /// The envelope half of [`Self::absorb`] — mirrors the SESSION-scoped
    /// part of `AppModel::handle_envelope` (screen flips excluded).
    fn absorb_envelope(&mut self, payload: &haider_protocol::EventPayload) {
        use haider_protocol::EventPayload;
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
            fn route(chips: &mut [ChipModel], payload: &EventPayload) {
                for chip in chips {
                    chip.transcript.apply(payload);
                    route(&mut chip.children, payload);
                }
            }
            route(&mut self.chips, payload);
        }
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
