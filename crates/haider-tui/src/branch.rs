//! Per-session branch state (B2b m1) — warm views under ONE session cursor.
//!
//! The daemon's branch core (B2a) made branches durable: a receipt-backed
//! `branch.create`, a `BranchCreated` journal fact, and branch-stamped
//! envelopes. This module is the TUI's half:
//!
//! * the checked-out projection stays the session's SOLE cursor authority —
//!   `SessionProjection::admit` is untouched law, and envelope `seq` is
//!   session-global, so per-branch cursors would report false gaps the
//!   moment sibling traffic interleaves (research risk 3);
//! * every NAMED branch owns a warm [`BranchSurfaces`] (projection + chips)
//!   materialized from `BranchCreated` and branch-stamped envelopes, active
//!   session and background slots alike — switching is immediate, never a
//!   reattach or rewind;
//! * the branch registry and the fork-coordinate tracker are COMMAND state,
//!   not display state: they record even when `render.ui == false` (the
//!   never-mutates-display law keeps holding for display surfaces — both
//!   halves are pinned in `tests/b2b_branch_state_tests.rs`);
//! * the daemon's `BranchCreated` fact is the ONLY branch materializer —
//!   receipt correlation installs nothing locally (no live branch before
//!   daemon truth; a typed failure leaves topology unchanged).
//!
//! The legacy/main branch is `None` everywhere, exactly as the wire has it:
//! inventing a concrete main id while old envelopes stay `None` would
//! create two mains (research Q1).

use std::collections::HashMap;

use haider_protocol::EventPayload;
use haider_protocol::branch::{BranchCreated, BranchDescriptor};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{AgentId, BranchId, NodeId};

use crate::app::ChipModel;
use crate::projection::SessionProjection;

/// The display surfaces one branch owns: transcript/run/footprint/menus/
/// todos live inside the projection; subagent chips ride beside it. A
/// branch switch swaps these as ONE unit (risk 8: one atomic authority).
#[derive(Debug, Default)]
pub struct BranchSurfaces {
    pub projection: SessionProjection,
    pub chips: Vec<ChipModel>,
}

/// One named branch: its durable descriptor plus its parked surfaces.
///
/// While this branch is the session's ACTIVE one its surfaces are checked
/// out into the live fields (the model's, or the background slot's) and
/// this slot holds neutral leftovers — the same checkout model sessions
/// themselves use (see `crate::session`).
#[derive(Debug)]
pub struct BranchView {
    pub descriptor: BranchDescriptor,
    pub surfaces: BranchSurfaces,
}

/// Where one admitted content payload lands, branch-wise. Decided by
/// [`BranchState::scope_of`] — shared by the attached and background paths
/// so the two can never drift (the `classify` precedent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchScope {
    /// Session-global by TYPE (risk 6): aggregate lifecycle payloads route
    /// to every view FIRST, even off a branch-stamped envelope — otherwise
    /// session status disappears from all forks or pollutes main history.
    Aggregate,
    /// The session's active branch — the checked-out live surfaces.
    Active,
    /// The parked main view (a named branch is active; this envelope is
    /// legacy/main-scoped).
    ParkedMain,
    /// A warm inactive named branch.
    ParkedNamed(BranchId),
    /// A branch this session has never seen a `BranchCreated` for. Counted,
    /// never guessed into another branch's view (no fabrication).
    Orphan,
}

/// What [`BranchState::switch`] did — a refused switch names its reason so
/// no caller has to guess whether topology changed (it never does on
/// refusal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome {
    /// The surfaces swapped and the cursor transplanted.
    Switched,
    /// The target was already displayed — nothing moved.
    AlreadyActive,
    /// No such branch is installed — nothing moved.
    UnknownBranch,
}

/// What [`BranchState::note_admitted`] consumed from one admitted envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmittedNote {
    /// A `BranchCreated` topology fact installed a NEW branch. The payload
    /// is consumed — content routing must not run (it is not an
    /// `EventPayload` and would otherwise count as unknown).
    BranchInstalled(BranchId),
    /// A replayed `BranchCreated` for a branch already installed —
    /// consumed, nothing changed (replay installs ONE branch, law).
    BranchKnown,
    /// Not a topology fact; content routing proceeds.
    Content,
}

/// Per-session branch state, checked in/out with the session (the A→B→A
/// law rides this one struct: `AppModel` and `SessionState` each hold one
/// and the session checkout swaps it whole).
#[derive(Debug, Default)]
pub struct BranchState {
    /// Every named branch this session has materialized, in journal order.
    /// The daemon's `BranchCreated` fact is the only writer.
    registry: Vec<BranchView>,
    /// The DISPLAYED branch (`None` = legacy/main) — per-session display
    /// state, never a daemon routing authority: every mutation captures
    /// its branch at issuance instead of re-reading this later.
    active: Option<BranchId>,
    /// The main view's surfaces while a named branch is checked out.
    /// `Some` exactly when `active` is `Some`.
    parked_main: Option<BranchSurfaces>,
    /// COMMAND COORDINATES, not display state: the last committed
    /// `NodeCommitted` (node id, envelope seq) per branch, recorded EVEN
    /// when `render.ui == false` — `/branch new` forks at exactly these
    /// coordinates, and a fork point inferred from display text would be a
    /// fabricated identity (research item 3).
    tracker: HashMap<Option<BranchId>, (NodeId, u64)>,
    /// A fork receipt arrived before its `BranchCreated` fact: activate
    /// the branch the moment the journal installs it. Receipt correlation
    /// itself installs NOTHING (no live branch before daemon truth).
    pending_activation: Option<BranchId>,
    /// Branch-stamped envelopes for a branch never installed here —
    /// surfaced, never guessed into a view (honesty counter).
    orphan_events: u64,
}

impl BranchState {
    /// Command-state hooks for one ADMITTED envelope (`Apply` or `Skip`) —
    /// called after the cursor gate and before the `render.ui` display
    /// gate, because the fork-coordinate tracker and the branch registry
    /// are command/topology state, not display state (the exemption both
    /// law tests pin).
    pub fn note_admitted(&mut self, envelope: &RawEnvelope) -> AdmittedNote {
        match envelope
            .payload
            .get("type")
            .and_then(serde_json::Value::as_str)
        {
            Some("node_committed") => {
                if let Some(node) = envelope
                    .payload
                    .get("node")
                    .and_then(serde_json::Value::as_str)
                {
                    self.tracker.insert(
                        envelope.branch_id.clone(),
                        (NodeId::new(node), envelope.seq),
                    );
                }
                AdmittedNote::Content
            }
            Some("branch_created") => match BranchCreated::from_payload_value(&envelope.payload) {
                Some(created) if !self.contains(&created.branch.branch_id) => {
                    let id = created.branch.branch_id.clone();
                    self.registry.push(BranchView {
                        descriptor: created.branch,
                        surfaces: BranchSurfaces::default(),
                    });
                    AdmittedNote::BranchInstalled(id)
                }
                Some(_) => AdmittedNote::BranchKnown,
                // A future branch-event kind this build cannot decode:
                // tolerated like any unknown payload (forward-compat law).
                None => AdmittedNote::Content,
            },
            _ => AdmittedNote::Content,
        }
    }

    /// Where one admitted content payload lands (type first, then branch —
    /// the risk-6 law).
    #[must_use]
    pub fn scope_of(&self, payload: &EventPayload, branch: Option<&BranchId>) -> BranchScope {
        if is_aggregate(payload) {
            return BranchScope::Aggregate;
        }
        if branch == self.active.as_ref() {
            return BranchScope::Active;
        }
        match branch {
            None => BranchScope::ParkedMain,
            Some(id) if self.contains(id) => BranchScope::ParkedNamed(id.clone()),
            Some(_) => BranchScope::Orphan,
        }
    }

    /// Swap the displayed branch's surfaces with `target`'s — the ONE
    /// switch authority (risk 8). The session cursor is transplanted onto
    /// the incoming projection so admission continues through the
    /// checked-out projection unbroken: switching neither reattaches nor
    /// rewinds, and parked cursors are dead values, always overwritten.
    pub fn switch(
        &mut self,
        target: Option<&BranchId>,
        projection: &mut SessionProjection,
        chips: &mut Vec<ChipModel>,
    ) -> SwitchOutcome {
        if self.active.as_ref() == target {
            return SwitchOutcome::AlreadyActive;
        }
        let incoming = match target {
            None => self.parked_main.take().unwrap_or_default(),
            Some(id) => {
                let Some(view) = self
                    .registry
                    .iter_mut()
                    .find(|view| &view.descriptor.branch_id == id)
                else {
                    return SwitchOutcome::UnknownBranch;
                };
                std::mem::take(&mut view.surfaces)
            }
        };
        let cursor = projection.last_applied();
        let outgoing = BranchSurfaces {
            projection: std::mem::replace(projection, incoming.projection),
            chips: std::mem::replace(chips, incoming.chips),
        };
        if let Some(seq) = cursor {
            projection.set_last_applied(seq);
        }
        match self.active.take() {
            None => self.parked_main = Some(outgoing),
            Some(previous) => {
                if let Some(view) = self
                    .registry
                    .iter_mut()
                    .find(|view| view.descriptor.branch_id == previous)
                {
                    view.surfaces = outgoing;
                }
            }
        }
        self.active = target.cloned();
        SwitchOutcome::Switched
    }

    /// Apply one AGGREGATE payload to every parked view (the live view gets
    /// it through the session reducer). Lifecycle stays coherent on every
    /// branch: no fork loses session status, none is painted with another
    /// branch's content.
    pub fn apply_aggregate_to_parked(&mut self, payload: &EventPayload) {
        if let Some(main) = &mut self.parked_main {
            main.projection.apply(payload);
        }
        for view in &mut self.registry {
            view.surfaces.projection.apply(payload);
        }
    }

    /// The parked main view, when a named branch is checked out.
    pub fn parked_main_mut(&mut self) -> Option<&mut BranchSurfaces> {
        self.parked_main.as_mut()
    }

    /// A named branch's parked surfaces.
    pub fn view_mut(&mut self, id: &BranchId) -> Option<&mut BranchSurfaces> {
        self.registry
            .iter_mut()
            .find(|view| &view.descriptor.branch_id == id)
            .map(|view| &mut view.surfaces)
    }

    /// Consume a pending fork activation for `id`, if one is armed.
    pub fn take_pending_activation(&mut self, id: &BranchId) -> bool {
        if self.pending_activation.as_ref() == Some(id) {
            self.pending_activation = None;
            return true;
        }
        false
    }

    /// Arm activation for a fork whose receipt landed before its
    /// `BranchCreated` fact. Installs nothing.
    pub fn arm_activation(&mut self, id: BranchId) {
        self.pending_activation = Some(id);
    }

    /// Count one branch-stamped envelope for a branch never installed.
    pub fn count_orphan(&mut self) {
        self.orphan_events += 1;
    }

    #[must_use]
    pub fn orphan_events(&self) -> u64 {
        self.orphan_events
    }

    #[must_use]
    pub fn contains(&self, id: &BranchId) -> bool {
        self.registry
            .iter()
            .any(|view| &view.descriptor.branch_id == id)
    }

    /// The displayed branch (`None` = main).
    #[must_use]
    pub fn active(&self) -> Option<&BranchId> {
        self.active.as_ref()
    }

    /// The displayed branch's NAME — `None` means main (the status bar and
    /// the picker both read this vocabulary).
    #[must_use]
    pub fn active_name(&self) -> Option<&str> {
        let active = self.active.as_ref()?;
        self.registry
            .iter()
            .find(|view| &view.descriptor.branch_id == active)
            .map(|view| view.descriptor.name.as_str())
    }

    /// Every named branch's descriptor, in journal order.
    pub fn descriptors(&self) -> impl Iterator<Item = &BranchDescriptor> {
        self.registry.iter().map(|view| &view.descriptor)
    }

    /// A named branch by its display name.
    #[must_use]
    pub fn find_by_name(&self, name: &str) -> Option<&BranchDescriptor> {
        self.registry
            .iter()
            .map(|view| &view.descriptor)
            .find(|descriptor| descriptor.name == name)
    }

    /// DERIVED branch count: main plus every named branch — never a stored
    /// scalar copy (the brief's item 1).
    #[must_use]
    pub fn named_count(&self) -> usize {
        self.registry.len()
    }

    /// The active branch's last committed node — the EXACT coordinates a
    /// `/branch new` fork issues (command coordinates, not display state).
    #[must_use]
    pub fn fork_point(&self) -> Option<&(NodeId, u64)> {
        self.tracker.get(&self.active)
    }

    /// Live subagents across EVERY view — the launcher aggregate counts
    /// all branches (law), so inactive-branch children keep a session hot.
    #[must_use]
    pub fn parked_live(&self) -> usize {
        self.parked_main
            .iter()
            .map(|view| crate::app::tree_live_count(&view.chips))
            .sum::<usize>()
            + self
                .registry
                .iter()
                .map(|view| crate::app::tree_live_count(&view.surfaces.chips))
                .sum::<usize>()
    }
}

/// Session-global BY TYPE (risk 6): aggregate lifecycle payloads are routed
/// before branch scope is even consulted. Everything else is branch content
/// (run states and run-scoped UI use their accepted branch — research Q2).
#[must_use]
pub const fn is_aggregate(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::HarnessStatus(_) | EventPayload::SessionState(_) | EventPayload::IdleDecayed
    )
}

/// Apply one admitted content payload to a PARKED view — the branch twin of
/// `SessionState::absorb_scoped`, sharing `classify` so the attached,
/// background and parked paths can never diverge. Session-wide bookkeeping
/// (turn flags, screen flips) is the caller's: a parked view owns only its
/// own surfaces.
pub fn absorb_into_view(
    view: &mut BranchSurfaces,
    payload: &EventPayload,
    agent: Option<&AgentId>,
) {
    use crate::session::Destination;
    match crate::session::classify(&mut view.projection, &view.chips, payload, agent) {
        Destination::Agent => crate::session::apply_agent_payload(&mut view.chips, payload),
        Destination::Chip(target) => {
            if let Some(chip) = crate::app::find_chip_mut(&mut view.chips, &target) {
                chip.transcript.apply(payload);
            }
        }
        Destination::Session => {
            if let EventPayload::RunState(state) = payload
                && state.is_terminal()
            {
                // Same law as both session paths: the ♪ tag ends where the
                // turn ends.
                view.projection.set_voice_live(false);
            }
            view.projection.apply(payload);
            if matches!(payload, EventPayload::MenuAnswered(_)) {
                crate::session::broadcast(&mut view.chips, payload);
            }
        }
    }
}
