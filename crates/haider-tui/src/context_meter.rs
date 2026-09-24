//! The session context meter — ONE pure resolution shared by the status
//! line, the `/tokens` (⌃G) context panel and the status mirror.
//!
//! 973-context-meter (owner 2026-09-24: "the context limit and % … are
//! wrong per model — it's always full 100%"). Root cause: the live identity
//! was seeded with the profile's OUTPUT budget (4,096 tokens) as its context
//! window, and a model whose catalog row declares no window kept that seed,
//! so any real prompt read `100% of 4.1k`. The meter now resolves:
//!
//! * **window** — the CURRENT model's declared window (the identity figure,
//!   `0` = unknown), else the window the daemon stamped on the latest
//!   snapshot; never an output budget, never a borrowed number. Unknown is
//!   rendered as unknown: no percentage is computed against a guess.
//! * **used** — the latest request-boundary snapshot's `used_tokens`: the
//!   last provider request's prompt (uncached + cached input) plus its reply,
//!   which becomes part of the next prompt. This is exactly the quantity the
//!   daemon compares with its compaction thresholds. Never a cumulative
//!   billing sum.
//! * **auto-compaction trigger** — the daemon's own threshold from the
//!   snapshot when that snapshot was taken against the displayed window;
//!   after a model switch (before the new model's first snapshot) the SAME
//!   protocol law ([`haider_protocol::context::context_soft_threshold_tokens`])
//!   projects it, marked as projected.

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
    /// The daemon's estimate of turns left before the trigger.
    pub turns_to_threshold: Option<u64>,
    /// The snapshot was a local estimate, not provider-reported usage.
    pub estimated: bool,
    /// A daemon snapshot backs `used_tokens` (else it is the usage fallback).
    pub from_snapshot: bool,
}

impl ContextMeter {
    /// Resolves the meter.
    ///
    /// `identity_window` is the current model's declared window (`0` =
    /// unknown). `snapshot_window_allowed` lets the latest snapshot's window
    /// stand in when the identity's is unknown (false when the catalog lists
    /// the current model without a window). `fallback_used` is only
    /// consulted without a snapshot.
    /// `default_reserved_output` is the output budget a session requests,
    /// used to project a threshold when no snapshot carries the daemon's
    /// actual reservation.
    #[must_use]
    pub fn resolve(
        footprint: Option<&ContextFootprint>,
        fallback_used: u64,
        identity_window: u64,
        snapshot_window_allowed: bool,
        default_reserved_output: impl Fn(u64) -> u64,
    ) -> Self {
        let snapshot_window = footprint
            .and_then(|footprint| footprint.context_window)
            .filter(|window| *window > 0);
        let window = (identity_window > 0)
            .then_some(identity_window)
            .or(snapshot_window.filter(|_| snapshot_window_allowed));
        let (auto_compact_at, threshold_projected) = match (window, footprint) {
            (Some(window), Some(footprint)) if snapshot_window == Some(window) => {
                (footprint.soft_threshold_tokens, false)
            }
            (Some(window), footprint) => {
                let reserved = footprint.map_or_else(
                    || default_reserved_output(window),
                    |f| f.reserved_output_tokens,
                );
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
                turns_to_threshold: (!threshold_projected)
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
                    " (projected until this model's first turn)"
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

/// Whole percent of `window`, rounded half up — the ONE rounding every
/// context surface uses (the meter, the panel and the compaction note).
#[must_use]
pub fn percent_of(tokens: u64, window: u64) -> u64 {
    let window = u128::from(window.max(1));
    u64::try_from((u128::from(tokens) * 100 + window / 2) / window).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn footprint(
        input: u64,
        cached: u64,
        output: u64,
        window: Option<u64>,
        reserved: u64,
    ) -> ContextFootprint {
        ContextFootprint {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            used_tokens: input + cached + output,
            context_window: window,
            reserved_output_tokens: reserved,
            soft_threshold_tokens: window.and_then(|window| {
                haider_protocol::context::context_soft_threshold_tokens(window, reserved)
            }),
            estimated_turns_to_threshold: None,
            truth: ContextFootprintTruth::Exact,
            accounting: None,
        }
    }

    fn cap(window: u64) -> u64 {
        crate::live::session_output_cap(window)
    }

    /// The owner's bug: an output budget standing in for the window. The
    /// profile seed (4,096) must never be used as a window; a live model with
    /// no declared window reads UNKNOWN, not 100%.
    #[test]
    fn regression_an_unknown_window_never_reads_full() {
        let snapshot = footprint(1_200, 18_000, 400, None, 30_000);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, true, cap);
        assert_eq!(meter.used_tokens, 19_600);
        assert_eq!(meter.window, None);
        assert_eq!(meter.percent(), None);
        assert_eq!(meter.status_text(10), "20k tok · window unknown");
        assert!(!meter.status_text(10).contains("100%"));
    }

    /// After a switch to a model the catalog lists WITHOUT a window, the
    /// previous model's snapshot window must not stand in for it.
    #[test]
    fn a_listed_model_without_a_window_ignores_the_previous_snapshot_window() {
        let snapshot = footprint(2_000, 150_000, 1_000, Some(1_000_000), 30_000);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, false, cap);
        assert_eq!(meter.window, None);
        assert_eq!(meter.auto_compact_at, None);
        assert_eq!(meter.status_text(10), "153k tok · window unknown");
    }

    #[test]
    fn per_model_windows_and_percentages() {
        // (model, window, prompt uncached, cached, reply, percent, trigger %)
        let cases: [(&str, u64, u64, u64, u64, u64, u64); 6] = [
            ("claude-opus-5-5", 1_000_000, 2_000, 150_000, 1_000, 15, 85),
            ("claude-sonnet-4-6", 200_000, 1_200, 18_000, 400, 10, 85),
            ("gpt-5.6-sol", 272_000, 5_000, 40_000, 2_000, 17, 85),
            ("gpt-6-sol", 400_000, 3_000, 120_000, 1_500, 31, 85),
            ("gemini-3-pro", 1_048_576, 10_000, 0, 500, 1, 85),
            // A 128k model: the output reservation (30k) caps the trigger
            // below 85% — the hard fit wins (98k = 77%).
            ("deepseek-v4-flash", 128_000, 60_000, 30_000, 1_000, 71, 77),
        ];
        for (model, window, input, cached, reply, percent, trigger) in cases {
            let snapshot = footprint(input, cached, reply, Some(window), cap(window));
            let meter = ContextMeter::resolve(Some(&snapshot), 0, window, true, cap);
            assert_eq!(meter.window, Some(window), "{model}");
            assert_eq!(meter.prompt_tokens, Some(input + cached), "{model}");
            assert_eq!(meter.percent(), Some(percent), "{model}");
            assert_eq!(meter.auto_compact_percent(), Some(trigger), "{model}");
            assert!(!meter.threshold_projected, "{model}");
        }
    }

    #[test]
    fn a_model_switch_rebases_window_and_projects_the_new_trigger() {
        // Snapshot taken on a 1M model; the user switched to a 200k model.
        let snapshot = footprint(2_000, 150_000, 1_000, Some(1_000_000), 30_000);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
        assert_eq!(meter.window, Some(200_000));
        assert_eq!(meter.percent(), Some(77));
        assert_eq!(meter.auto_compact_at, Some(170_000));
        assert!(meter.threshold_projected);
        assert_eq!(meter.status_text(10), "153k tok · ▰▰▰▰▰▰▰▰▱▱ 77% of 200k");
    }

    #[test]
    fn the_snapshot_window_serves_until_the_catalog_arrives() {
        let snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, true, cap);
        assert_eq!(meter.window, Some(200_000));
        assert_eq!(meter.auto_compact_at, Some(170_000));
        assert!(!meter.threshold_projected);
    }

    #[test]
    fn before_the_first_turn_the_trigger_is_projected_from_the_window() {
        let meter = ContextMeter::resolve(None, 0, 400_000, true, cap);
        assert_eq!(meter.status_text(10), "0 tok · ▱▱▱▱▱▱▱▱▱▱ 0% of 400k");
        assert_eq!(meter.auto_compact_at, Some(340_000));
        assert!(meter.threshold_projected);
        assert_eq!(
            meter.detail_lines(),
            [
                "no request snapshot yet",
                "auto-compact at 340k (85%) (projected until this model's first turn)"
            ]
        );
    }

    #[test]
    fn an_overfull_context_reads_over_one_hundred_percent() {
        let snapshot = footprint(210_000, 0, 0, Some(200_000), 30_000);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
        assert_eq!(meter.percent(), Some(105));
    }

    #[test]
    fn detail_defines_prompt_cached_and_reply() {
        let mut snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
        snapshot.estimated_turns_to_threshold = Some(12);
        let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
        assert_eq!(
            meter.detail_lines(),
            [
                "last prompt 19k (cached 18k) + reply 400",
                "auto-compact at 170k (85%)",
                "≈12 turns to auto-compaction"
            ]
        );
    }
}
