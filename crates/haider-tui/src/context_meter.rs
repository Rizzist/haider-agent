//! The session context meter — ONE pure resolution shared by the status
//! line, the `/tokens` (⌃G) context panel and the status mirror.
//!
//! 973-context-meter (owner 2026-09-24: "the context limit and % … are
//! wrong per model — it's always full 100%"). Root cause: the live identity
//! was seeded with the profile's OUTPUT budget (4,096 tokens) as its context
//! window, and a model whose catalog row declares no window kept that seed,
//! so any real prompt read `100% of 4.1k`. The meter now resolves:
//!
//! * **window** — the current-epoch request's own stamped window when one
//!   exists, else the current model's declared window (`0` = unknown);
//!   never an output budget or a previous selection's window. Unknown is
//!   rendered as unknown: no percentage is computed against a guess.
//! * **used** — the latest request-boundary snapshot's `used_tokens`: the
//!   last provider request's prompt (uncached + cached input) plus its reply,
//!   which becomes part of the next prompt. This is exactly the quantity the
//!   daemon compares with its compaction thresholds. Never a cumulative
//!   billing sum.
//! * **auto-compaction trigger** — the daemon's own threshold from the
//!   snapshot when that snapshot was taken under the CURRENT model/output
//!   budget epoch against the displayed window; after a model or budget
//!   change (before the new epoch's first snapshot) the SAME protocol law
//!   ([`haider_protocol::context::context_soft_threshold_tokens`]) projects
//!   it from the new epoch's reservation, marked as projected.
//!
//! The epoch is SESSION state ([`MeterEpoch`]): it travels with the session
//! on checkout exactly like its projection, so reopening a session never
//! meters it against another session's model (973-context-meter-fixes).

use crate::format::{fmt_tok, meter_cells};
use haider_protocol::context::{ContextFootprint, ContextFootprintTruth};

/// A resolved meter. Every field is observed truth or explicitly absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMeter {
    /// Context occupancy: last request prompt + its reply (see module docs).
    pub used_tokens: u64,
    /// Last request prompt tokens, uncached + cached input. `None` without a
    /// daemon snapshot.
    pub prompt_tokens: Option<u64>,
    /// Cached subset of [`Self::prompt_tokens`].
    pub cached_tokens: Option<u64>,
    /// Last reply's output tokens.
    pub reply_tokens: Option<u64>,
    /// Model context window; `None` = unknown.
    pub window: Option<u64>,
    /// Automatic (model-summary) compaction trigger in tokens.
    pub auto_compact_at: Option<u64>,
    /// `true` when [`Self::auto_compact_at`] was projected locally for a
    /// newly selected model rather than read from a daemon snapshot.
    pub threshold_projected: bool,
    /// The new selection has not yet reported its effective output reserve.
    pub reserve_assumed: bool,
    /// The daemon's estimate of turns left before the trigger.
    pub turns_to_threshold: Option<u64>,
    /// The snapshot was a local estimate, not provider-reported usage.
    pub estimated: bool,
    /// A daemon snapshot backs `used_tokens` (else it is the usage fallback).
    pub from_snapshot: bool,
}

/// How the latest snapshot relates to the session's CURRENT model/output
/// budget epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotEpoch {
    /// Taken under the current model and output budget: its trigger and
    /// turns estimate are daemon truth, and its window may stand in for an
    /// undeclared one.
    Current,
    /// The request belongs to this selection and proves its window, but
    /// its output reserve conflicts with the committed budget. Recompute
    /// the trigger and discard its turns estimate.
    CurrentReserveMismatch,
    /// Taken before the last model or output-budget change: it describes
    /// the previous epoch. Its used figure still stands (the context is the
    /// same), but never its window, trigger, reservation or turns estimate.
    Previous,
}

/// The model/output-budget epoch ONE session's meter resolves against
/// (973-context-meter-fixes B1/B3). The viewed session's lives on the app
/// model; a parked session's in its slot, and it travels on checkout with
/// the session's projection — so session A never inherits session B's
/// model, and a change within a session rebases immediately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeterEpoch {
    /// The session's committed `(provider, model)`; `None` until known.
    pub pair: Option<(String, String)>,
    /// Daemon-committed selection sequence. `None` denotes an older daemon
    /// or a session whose own selection has not arrived yet.
    pub selection_epoch: Option<u64>,
    /// The committed effective output budget of this epoch (the daemon's
    /// model-selection reply, or the session's typed metadata); `None` =
    /// derive it from the model's declared maximum.
    pub output_budget: Option<u64>,
    /// A user budget survives model changes, subject to the daemon's clamp.
    pub user_budget: bool,
    /// A fact arrived before the reply or metadata containing its reserve.
    pub reserve_assumed: bool,
}

impl MeterEpoch {
    /// Only a newer daemon version advances selection. An equal version may
    /// fill its missing budget; an older or unknown version cannot undo it.
    pub fn admit(
        &mut self,
        pair: (String, String),
        selection_epoch: Option<u64>,
        output_budget: Option<u64>,
        user_budget: Option<bool>,
    ) -> bool {
        match (self.selection_epoch, selection_epoch) {
            (Some(current), Some(incoming)) if incoming < current => return false,
            (Some(current), Some(incoming)) if incoming == current => {
                if self.pair.as_ref().is_some_and(|held| held != &pair) {
                    return false;
                }
                if !self.reserve_assumed
                    && output_budget
                        .is_some_and(|budget| self.output_budget.is_some_and(|held| held != budget))
                {
                    return false;
                }
            }
            (Some(_), None) => return false,
            _ => {}
        }
        let newer = matches!((self.selection_epoch, selection_epoch),
            (Some(current), Some(incoming)) if incoming > current)
            || self.selection_epoch.is_none() && selection_epoch.is_some();
        if newer {
            if self.user_budget {
                self.reserve_assumed = output_budget.is_none();
            } else {
                self.output_budget = None;
                self.reserve_assumed = output_budget.is_none();
            }
        }
        self.pair = Some(pair);
        self.selection_epoch = selection_epoch;
        if let Some(budget) = output_budget {
            self.output_budget = Some(budget);
            self.reserve_assumed = false;
        }
        if let Some(user_budget) = user_budget {
            self.user_budget = user_budget;
        }
        true
    }

    /// A snapshot is current only when its request's version matches this
    /// session's committed selection. Missing provenance is never current.
    #[must_use]
    pub fn snapshot_epoch(&self, snapshot: Option<&ContextFootprint>) -> SnapshotEpoch {
        match (
            self.selection_epoch,
            snapshot.and_then(|item| item.selection_epoch),
        ) {
            (Some(current), Some(request)) if current == request => {
                if snapshot.is_some_and(|item| {
                    !self.reserve_assumed
                        && self
                            .output_budget
                            .is_some_and(|budget| item.reserved_output_tokens != budget)
                }) {
                    SnapshotEpoch::CurrentReserveMismatch
                } else {
                    SnapshotEpoch::Current
                }
            }
            _ => SnapshotEpoch::Previous,
        }
    }

    /// A surviving user reserve is provisional until this epoch's metadata
    /// arrives. Apply the new model's known output cap while projecting it.
    #[must_use]
    pub fn projected_output_budget(&self, declared_output_limit: Option<u64>) -> Option<u64> {
        self.output_budget.map(|budget| {
            if self.reserve_assumed && self.user_budget {
                budget.min(declared_output_limit.unwrap_or(budget))
            } else {
                budget
            }
        })
    }
}

impl ContextMeter {
    /// Resolves the meter.
    ///
    /// `identity_window` is the current model's declared window (`0` =
    /// unknown). `epoch` says whether the latest snapshot belongs to the
    /// current model/budget epoch (and whether its window may stand in for
    /// an undeclared one). `fallback_used` is only consulted without a
    /// snapshot. `epoch_reserved_output` is the current epoch's output
    /// reservation (the daemon's committed budget, else the derived one),
    /// used to project a trigger when no current snapshot carries the
    /// daemon's own.
    #[must_use]
    pub fn resolve(
        footprint: Option<&ContextFootprint>,
        fallback_used: u64,
        identity_window: u64,
        epoch: SnapshotEpoch,
        epoch_reserved_output: impl Fn(u64) -> u64,
    ) -> Self {
        let snapshot_window = footprint
            .and_then(|footprint| footprint.context_window)
            .filter(|window| *window > 0);
        let window = snapshot_window
            .filter(|_| epoch != SnapshotEpoch::Previous)
            .or((identity_window > 0).then_some(identity_window));
        // The daemon's trigger and turns estimate describe the snapshot's
        // model, budget AND window: they are current only when all three
        // are. Window equality alone is not proof of the epoch.
        let snapshot_governs =
            epoch == SnapshotEpoch::Current && window.is_some() && snapshot_window == window;
        let (auto_compact_at, threshold_projected) = match (window, footprint) {
            (Some(_), Some(footprint)) if snapshot_governs => {
                (footprint.soft_threshold_tokens, false)
            }
            (Some(window), _) => {
                let reserved = epoch_reserved_output(window);
                (
                    haider_protocol::context::context_soft_threshold_tokens(window, reserved),
                    true,
                )
            }
            (None, _) => (None, false),
        };
        match footprint {
            Some(footprint) => Self {
                used_tokens: footprint.used_tokens,
                prompt_tokens: Some(
                    footprint
                        .input_tokens
                        .saturating_add(footprint.cached_input_tokens),
                ),
                cached_tokens: Some(footprint.cached_input_tokens),
                reply_tokens: Some(footprint.output_tokens),
                window,
                auto_compact_at,
                threshold_projected,
                reserve_assumed: false,
                // The daemon's turns estimate is measured against the
                // SNAPSHOT's model, budget and window: it applies only while
                // that snapshot governs (never after a model or budget
                // change, never to an unknown window).
                turns_to_threshold: snapshot_governs
                    .then_some(footprint.estimated_turns_to_threshold)
                    .flatten(),
                estimated: footprint.truth == ContextFootprintTruth::Estimated,
                from_snapshot: true,
            },
            None => Self {
                used_tokens: fallback_used,
                prompt_tokens: None,
                cached_tokens: None,
                reply_tokens: None,
                window,
                auto_compact_at,
                threshold_projected,
                reserve_assumed: false,
                turns_to_threshold: None,
                estimated: false,
                from_snapshot: false,
            },
        }
    }

    /// Occupancy fraction for the meter cells; `None` for an unknown window.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(&self) -> Option<f64> {
        self.window
            .map(|window| self.used_tokens as f64 / window.max(1) as f64)
    }

    /// Whole percent of the window (rounded, NOT clamped: an over-full
    /// context reads over 100%). `None` for an unknown window.
    #[must_use]
    pub fn percent(&self) -> Option<u64> {
        self.window
            .map(|window| percent_of(self.used_tokens, window))
    }

    /// The trigger as a whole percent of the window.
    #[must_use]
    pub fn auto_compact_percent(&self) -> Option<u64> {
        Some(percent_of(self.auto_compact_at?, self.window?))
    }

    /// `~` when the used figure is an estimate.
    #[must_use]
    pub fn approx(&self) -> &'static str {
        if self.estimated { "~" } else { "" }
    }

    /// The status-line meter text.
    ///
    /// Known window: `20k tok · ▰▱▱▱▱▱▱▱▱▱ 10% of 200k · compact at 85%`.
    /// Unknown: `20k tok · window unknown`. The one-line bar carries only
    /// the DAEMON's trigger (from a snapshot against this window); a trigger
    /// projected for a newly selected model is shown by the `/tokens` panel.
    #[must_use]
    pub fn status_text(&self, cells: usize) -> String {
        let used = format!("{}{} tok", self.approx(), fmt_tok(self.used_tokens));
        let (Some(window), Some(fraction), Some(percent)) =
            (self.window, self.fraction(), self.percent())
        else {
            return format!("{used} · window unknown");
        };
        let mut text = format!(
            "{used} · {} {percent}% of {}",
            meter_cells(fraction, cells),
            fmt_tok(window)
        );
        if let Some(trigger) = self.auto_compact_percent()
            && !self.threshold_projected
        {
            text.push_str(&format!(" · compact at {trigger}%"));
        }
        text
    }

    /// The `/tokens` panel's detail lines: the definition of every number.
    #[must_use]
    pub fn detail_lines(&self) -> Vec<String> {
        let approx = self.approx();
        let mut parts = Vec::new();
        match (self.prompt_tokens, self.cached_tokens, self.reply_tokens) {
            (Some(prompt), Some(cached), Some(reply)) => {
                parts.push(format!(
                    "last prompt {approx}{} (cached {approx}{}) + reply {approx}{}",
                    fmt_tok(prompt),
                    fmt_tok(cached),
                    fmt_tok(reply)
                ));
            }
            _ => parts.push("no request snapshot yet".to_owned()),
        }
        match (self.auto_compact_at, self.auto_compact_percent()) {
            (Some(at), Some(percent)) => {
                let projected = if self.threshold_projected {
                    if self.reserve_assumed {
                        " (projected; output reserve pending confirmation)"
                    } else {
                        " (projected until this model's first turn)"
                    }
                } else {
                    ""
                };
                parts.push(format!(
                    "auto-compact at {} ({percent}%){projected}",
                    fmt_tok(at)
                ));
            }
            _ if self.window.is_none() => {
                parts.push("window unknown — no auto-compaction trigger".to_owned());
            }
            _ => {}
        }
        if let Some(turns) = self.turns_to_threshold {
            parts.push(format!("≈{turns} turns to auto-compaction"));
        }
        parts
    }
}

/// The output reservation a session on this model carries when no snapshot
/// reports the daemon's actual one: the daemon derives `min(default, model
/// maximum)` for a `session.create` that sends zero (973-output-cap), so the
/// projected trigger uses the same reservation. Bounded by a known window.
#[must_use]
pub fn derived_reserved_output(declared_output_limit: Option<u64>, window: u64) -> u64 {
    let default = haider_protocol::output_budget::DEFAULT_OUTPUT_LIMIT;
    let reserved = default.min(declared_output_limit.unwrap_or(default));
    if window == 0 {
        reserved
    } else {
        reserved.min(window)
    }
}

/// The current epoch's output reservation: the daemon's COMMITTED effective
/// budget when the session's epoch carries one (a user-set budget survives
/// or is clamped there, which no client derivation can know), else the
/// derivation from the model's declared maximum.
#[must_use]
pub fn epoch_reserved_output(
    committed: Option<u64>,
    declared_output_limit: Option<u64>,
    window: u64,
) -> u64 {
    committed.unwrap_or_else(|| derived_reserved_output(declared_output_limit, window))
}

/// Whole percent of `window`, rounded half up — the ONE rounding every
/// context surface uses (the meter, the panel and the compaction note).
#[must_use]
pub fn percent_of(tokens: u64, window: u64) -> u64 {
    let window = u128::from(window.max(1));
    u64::try_from((u128::from(tokens) * 100 + window / 2) / window).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "context_meter_tests.rs"]
mod tests;
