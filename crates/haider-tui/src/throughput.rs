//! W-G live token-throughput tracker: a PURE sampler over a ring buffer that
//! turns the streaming output-token count into a status row —
//! `Throughput ▁▂▃▄▅ 126 tps · μ 119 · p95 154` — shown above the composer
//! while a turn streams.
//!
//! Design law (WG1/WG6): every displayed figure is a pure function of the
//! sample buffer. The tracker is fed `observe(now_ms, cumulative_output_tokens,
//! exact)` from the model on the existing anim/frame clock — NO new timer — and
//! computes an instantaneous windowed rate plus per-turn aggregate stats. The
//! caller injects `now_ms` (`AppModel::clock_ms`), so tests seed a scripted
//! stream over mock time and the ladder/probe replays reproduce byte-for-byte.
//!
//! Honesty (WG5): when the provider reports incremental usage the readout is
//! exact; when it does not, the model feeds an APPROXIMATE token count derived
//! from streamed text and the readout wears a leading `~`. It never fabricates
//! an exact figure it did not measure.

use std::collections::VecDeque;

/// The unicode block ramp, low → high, that maps a sample's magnitude to a
/// sparkline column (decision 3).
pub const SPARK_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Per-turn tps samples kept for the sparkline + aggregate stats (decision 3,
/// N in the 16–24 band).
const SAMPLE_CAP: usize = 24;
/// The sparkline's FIXED rendered width (owner 2026-08-15): the readout box
/// never grows — samples enter at the right edge and roll off the left, and
/// until the ring fills the missing columns are blank (never a fabricated
/// floor glyph).
pub const SPARK_WIDTH: usize = SAMPLE_CAP;
/// Raw `(t_ms, cumulative_tokens)` observations retained for the windowed rate.
/// Bounded well above one window's worth at the anim cadence.
const RAW_CAP: usize = 96;
/// The sliding window (ms) the instantaneous rate is measured over
/// (decision 1: "last ~1s of deltas").
const WINDOW_MS: u64 = 1_000;
/// A new tps sample joins the ring at most this often, so the 24-slot
/// sparkline spans ~6s of history rather than a fraction of a second at the
/// 30fps clock. Raw observations are still recorded every tick.
const SAMPLE_INTERVAL_MS: u64 = 250;
/// Samples required before μ/p95 are shown (decision 5: show the current rate
/// alone until enough exist, never fake the aggregates).
const STATS_MIN: usize = 4;

/// The pure, styleable readout — the render and plain layers both build their
/// line from these fields, so the two never drift (WG6 parity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThroughputReadout {
    /// The sparkline over the current sample ring.
    pub spark: String,
    /// The current windowed rate, integer tps.
    pub tps: u32,
    /// True when the rate is derived from an approximate token count (the
    /// provider reported no incremental usage this turn) — the `~` marker.
    pub approx: bool,
    /// Mean of the ring, once `STATS_MIN` samples exist.
    pub mean: Option<u32>,
    /// 95th percentile of the ring, once `STATS_MIN` samples exist.
    pub p95: Option<u32>,
}

impl ThroughputReadout {
    /// The plain-mode line — the WG6 parity anchor. The styled render builds
    /// the SAME glyphs and numbers with theme spans; this is the greppable
    /// oracle for both.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let tilde = if self.approx { "~" } else { "" };
        let mut line = format!("Throughput {} {tilde}{} tps", self.spark, self.tps);
        if let Some(mean) = self.mean {
            line.push_str(&format!(" · μ {mean}"));
        }
        if let Some(p95) = self.p95 {
            line.push_str(&format!(" · p95 {p95}"));
        }
        line
    }

    /// Compact form for the composer identity line — no verbose label, just
    /// the sparkline, rate, and mean (p95 is dropped; the line is shared with
    /// the model identity). The `tps` token anchors it for parity greps.
    #[must_use]
    pub fn pill_text(&self) -> String {
        let tilde = if self.approx { "~" } else { "" };
        let mut line = format!("{} {tilde}{} tps", self.spark, self.tps);
        if let Some(mean) = self.mean {
            line.push_str(&format!(" · μ{mean}"));
        }
        line
    }
}

/// The per-turn sampler. Fed cumulative output-token counts on the frame clock;
/// resets itself when the count regresses (a new turn's cumulative restarts)
/// and on an explicit idle [`Self::reset`].
#[derive(Debug, Clone, Default)]
pub struct ThroughputTracker {
    /// `(t_ms, cumulative_tokens)` observations within (roughly) one window.
    raw: VecDeque<(u64, u64)>,
    /// Recent per-interval tps samples for the sparkline + stats.
    samples: VecDeque<u32>,
    /// The last cumulative token count seen — a regression means a new turn.
    last_tokens: Option<u64>,
    /// The most recent windowed rate (the live number), independent of the
    /// ring cadence.
    last_tps: Option<u32>,
    /// When the last ring sample was appended (cadence gate).
    last_sample_ms: Option<u64>,
    /// Sticky for the turn: once ANY exact-usage observation lands, the readout
    /// stops flagging itself approximate — we have real numbers now.
    exact_seen: bool,
}

impl ThroughputTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the tracker holds no state — the idle resting shape. An idle
    /// [`Self::reset`] returns it here and further idle ticks are no-ops, so
    /// idle frames stay byte-identical (WG3).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
            && self.samples.is_empty()
            && self.last_tokens.is_none()
            && self.last_tps.is_none()
            && self.last_sample_ms.is_none()
            && !self.exact_seen
    }

    /// Clear all per-turn state (idle, or a new turn).
    pub fn reset(&mut self) {
        self.raw.clear();
        self.samples.clear();
        self.last_tokens = None;
        self.last_tps = None;
        self.last_sample_ms = None;
        self.exact_seen = false;
    }

    /// Record one observation of the cumulative output-token count at `now_ms`.
    /// `exact` is true when the count came from provider-reported usage, false
    /// when it is an approximate (text-derived) estimate.
    ///
    /// A cumulative count that goes BACKWARDS marks a new turn and resets the
    /// buffers before recording the fresh baseline (WG4 reset), which also
    /// keeps the exact→approx source jitter from ever fabricating a rate.
    pub fn observe(&mut self, now_ms: u64, tokens: u64, exact: bool) {
        if self.last_tokens.is_some_and(|prev| tokens < prev) {
            self.reset();
        }
        self.last_tokens = Some(tokens);
        if exact {
            self.exact_seen = true;
        }

        self.raw.push_back((now_ms, tokens));
        while self.raw.len() > RAW_CAP {
            self.raw.pop_front();
        }
        // Evict past the window, but always keep a pair to rate over.
        while self.raw.len() > 2
            && now_ms.saturating_sub(self.raw.front().map_or(now_ms, |&(t, _)| t)) > WINDOW_MS
        {
            self.raw.pop_front();
        }

        if let Some(tps) = windowed_tps(&self.raw) {
            self.last_tps = Some(tps);
            let due = self
                .last_sample_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= SAMPLE_INTERVAL_MS);
            if due {
                self.samples.push_back(tps);
                while self.samples.len() > SAMPLE_CAP {
                    self.samples.pop_front();
                }
                self.last_sample_ms = Some(now_ms);
            }
        }
    }

    /// The current readout, or `None` when no rate has been established yet
    /// (fewer than two timed observations). The render layer additionally
    /// gates on the run being in a streaming state (WG3).
    #[must_use]
    pub fn readout(&self) -> Option<ThroughputReadout> {
        let tps = self.last_tps?;
        let ring: Vec<u32> = self.samples.iter().copied().collect();
        let enough = ring.len() >= STATS_MIN;
        // Fixed-width roll: left-pad the young ring with blanks so the box
        // is SPARK_WIDTH columns from the first frame and samples march
        // right-to-left as the ring fills and caps.
        let drawn = spark(&ring, SPARK_WIDTH);
        let pad = SPARK_WIDTH.saturating_sub(drawn.chars().count());
        let spark = format!("{}{drawn}", " ".repeat(pad));
        Some(ThroughputReadout {
            spark,
            tps,
            approx: !self.exact_seen,
            mean: enough.then(|| mean(&ring)).flatten(),
            p95: enough.then(|| percentile(&ring, 95)).flatten(),
        })
    }

    /// Test-only view of the ring contents.
    #[cfg(test)]
    #[must_use]
    pub fn samples(&self) -> Vec<u32> {
        self.samples.iter().copied().collect()
    }
}

/// The instantaneous windowed rate over the raw `(t, cumulative_tokens)`
/// buffer: the token delta across the retained span, per wall-second. `None`
/// until a positive time span exists between two observations.
#[must_use]
fn windowed_tps(raw: &VecDeque<(u64, u64)>) -> Option<u32> {
    if raw.len() < 2 {
        return None;
    }
    let &(t0, tok0) = raw.front()?;
    let &(t1, tok1) = raw.back()?;
    let dt = t1.checked_sub(t0).filter(|d| *d > 0)?;
    let dtok = tok1.saturating_sub(tok0);
    Some(u32::try_from(dtok.saturating_mul(1_000) / dt).unwrap_or(u32::MAX))
}

/// Map a sample buffer to a sparkline of at most `width` columns (the last
/// `width` samples), each column scaled across the window's own [min, max]
/// range so a rising series ramps monotonically and a flat series renders
/// flat-low (WG2). Empty in → empty out; never panics.
#[must_use]
pub fn spark(samples: &[u32], width: usize) -> String {
    if samples.is_empty() || width == 0 {
        return String::new();
    }
    let show = &samples[samples.len().saturating_sub(width)..];
    let max = show.iter().copied().max().unwrap_or(0);
    let min = show.iter().copied().min().unwrap_or(0);
    let top = (SPARK_RAMP.len() - 1) as u64;
    // A flat window (max == min, includes all-zero) renders as the lowest
    // glyph — a level line, not a fabricated ramp.
    if max == min {
        return SPARK_RAMP[0].to_string().repeat(show.len());
    }
    let span = u64::from(max - min);
    show.iter()
        .map(|&v| {
            let offset = u64::from(v - min);
            // Rounded nearest-glyph, clamped into the ramp.
            let idx = ((offset * top) + span / 2) / span;
            SPARK_RAMP[(idx as usize).min(SPARK_RAMP.len() - 1)]
        })
        .collect()
}

/// The integer mean of the samples (rounded), or `None` when empty (WG1).
#[must_use]
pub fn mean(samples: &[u32]) -> Option<u32> {
    if samples.is_empty() {
        return None;
    }
    let sum: u64 = samples.iter().map(|&v| u64::from(v)).sum();
    let n = samples.len() as u64;
    Some(u32::try_from((sum + n / 2) / n).unwrap_or(u32::MAX))
}

/// The `p`-th percentile of the samples by the nearest-rank method: sort, take
/// rank `⌈p/100 · n⌉` (1-based), clamped into range (WG1). On a 1..=100
/// distribution `percentile(_, 95) == 95`.
#[must_use]
pub fn percentile(samples: &[u32], p: u8) -> Option<u32> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<u32> = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    // ⌈p/100 · n⌉ in integer arithmetic, then clamp to [1, n].
    let rank = (usize::from(p) * n).div_ceil(100).clamp(1, n);
    Some(sorted[rank - 1])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn feed(tracker: &mut ThroughputTracker, samples: &[(u64, u64, bool)]) {
        for &(t, tok, exact) in samples {
            tracker.observe(t, tok, exact);
        }
    }

    // ---- WG1: pure tps / μ / p95 ----

    #[test]
    fn wg1_percentile_is_nearest_rank_and_correct_on_a_known_distribution() {
        let dist: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&dist, 95), Some(95));
        assert_eq!(percentile(&dist, 50), Some(50));
        assert_eq!(percentile(&dist, 100), Some(100));
        assert_eq!(percentile(&dist, 1), Some(1));
        // Order-invariant: the same multiset percentiles identically.
        let mut shuffled = dist.clone();
        shuffled.rotate_left(37);
        shuffled.swap(0, 61);
        assert_eq!(percentile(&shuffled, 95), Some(95));
        // Degenerate sizes never panic.
        assert_eq!(percentile(&[], 95), None);
        assert_eq!(percentile(&[7], 95), Some(7));
    }

    #[test]
    fn wg1_mean_rounds_and_is_pure() {
        assert_eq!(mean(&[10, 20, 30]), Some(20));
        // 3/2 rounds to 2 (half-up).
        assert_eq!(mean(&[1, 2]), Some(2));
        assert_eq!(mean(&[]), None);
        // Fixed buffer → fixed output (purity).
        let buf = [100, 110, 120, 130];
        assert_eq!(mean(&buf), mean(&buf));
    }

    #[test]
    fn wg1_windowed_rate_is_tokens_per_wall_second() {
        // 120 tokens over exactly one second → 120 tps.
        let mut raw = VecDeque::new();
        raw.push_back((1_000, 0));
        raw.push_back((2_000, 120));
        assert_eq!(windowed_tps(&raw), Some(120));
        // Zero elapsed → no rate (never divides by zero).
        let mut flat_time = VecDeque::new();
        flat_time.push_back((5_000, 10));
        flat_time.push_back((5_000, 40));
        assert_eq!(windowed_tps(&flat_time), None);
        // A single observation has no rate.
        let mut one = VecDeque::new();
        one.push_back((0, 0));
        assert_eq!(windowed_tps(&one), None);
    }

    // ---- WG2: sparkline mapping ----

    #[test]
    fn wg2_sparkline_ramps_monotonically_and_never_panics() {
        // A rising series maps to a rising ramp: min→▁, max→█.
        let ramp = spark(&[1, 2, 3, 4, 5, 6, 7, 8], 24);
        assert_eq!(ramp.chars().next(), Some('▁'));
        assert_eq!(ramp.chars().last(), Some('█'));
        // Monotonic non-decreasing glyph indices.
        let idxs: Vec<usize> = ramp
            .chars()
            .map(|c| SPARK_RAMP.iter().position(|&r| r == c).unwrap())
            .collect();
        assert!(idxs.windows(2).all(|w| w[0] <= w[1]), "{ramp}");
        // Empty / zero-width → empty, no panic.
        assert_eq!(spark(&[], 24), "");
        assert_eq!(spark(&[5, 5, 5], 0), "");
    }

    #[test]
    fn wg2_flat_buffer_renders_flat_low() {
        // A constant series (incl. all-zero) is a level line at the floor glyph.
        assert_eq!(spark(&[42, 42, 42, 42], 24), "▁▁▁▁");
        assert_eq!(spark(&[0, 0, 0], 24), "▁▁▁");
    }

    #[test]
    fn wg2_width_shows_only_the_last_columns() {
        // width caps the column count to the most-recent samples.
        let s = spark(&[1, 2, 3, 4, 5, 6, 7, 8], 3);
        assert_eq!(s.chars().count(), 3);
    }

    // ---- WG4: streaming rise + per-turn reset ----

    #[test]
    fn wg4_scripted_stream_rises_and_populates_the_sparkline() {
        let mut tracker = ThroughputTracker::new();
        // A steadily accelerating token stream over mock time.
        let mut script = Vec::new();
        let mut tok = 0u64;
        for i in 0..24u64 {
            tok += 50 + i * 4; // rising per-step delta → rising tps
            script.push((250 * (i + 1), tok, true));
        }
        feed(&mut tracker, &script);
        let readout = tracker.readout().expect("a rate is established");
        assert!(readout.tps > 0);
        assert!(!readout.approx, "exact usage → no tilde");
        assert!(readout.mean.is_some());
        assert!(readout.p95.is_some());
        // Fixed-width law (owner 2026-08-15): the box is ALWAYS SPARK_WIDTH
        // columns — a young ring is left-padded with blanks, so samples
        // enter at the right and roll off the left.
        assert_eq!(readout.spark.chars().count(), SPARK_WIDTH);
        // The drawn portion is populated and its later columns out-rank its
        // first drawn column (padding blanks are not samples).
        let first = SPARK_RAMP
            .iter()
            .position(|&r| Some(r) == readout.spark.chars().find(|c| *c != ' '))
            .unwrap();
        let last = SPARK_RAMP
            .iter()
            .position(|&r| r == readout.spark.chars().last().unwrap())
            .unwrap();
        assert!(last >= first, "the rate ramps up: {}", readout.spark);
    }

    #[test]
    fn wg4_cumulative_regression_resets_the_turn() {
        let mut tracker = ThroughputTracker::new();
        feed(
            &mut tracker,
            &[(250, 100, true), (500, 260, true), (750, 450, true)],
        );
        assert!(tracker.readout().is_some());
        let before = tracker.samples().len();
        assert!(before >= 2);
        // A new turn: cumulative output restarts small → buffers clear.
        tracker.observe(1_000, 5, true);
        assert_eq!(
            tracker.samples().len(),
            0,
            "the regression cleared the ring"
        );
        // And it rebuilds cleanly from the new baseline.
        tracker.observe(1_250, 130, true);
        assert!(tracker.readout().is_some());
    }

    #[test]
    fn wg4_degrades_before_enough_samples() {
        let mut tracker = ThroughputTracker::new();
        // Two timed observations: a current rate exists, but μ/p95 do not yet.
        tracker.observe(250, 0, true);
        tracker.observe(500, 40, true);
        let readout = tracker.readout().expect("current rate is available");
        assert!(readout.tps > 0);
        assert_eq!(readout.mean, None, "μ withheld until STATS_MIN samples");
        assert_eq!(readout.p95, None, "p95 withheld until STATS_MIN samples");
    }

    // ---- WG5: fallback honesty ----

    #[test]
    fn wg5_approx_source_is_marked_and_never_fabricates_exact() {
        let mut tracker = ThroughputTracker::new();
        // A provider with no incremental usage: the model feeds approximate,
        // text-derived counts (exact = false).
        feed(
            &mut tracker,
            &[(250, 20, false), (500, 44, false), (750, 70, false)],
        );
        let readout = tracker.readout().expect("approx rate is available");
        assert!(readout.approx, "no exact usage → the ~ marker");
        assert!(readout.plain_text().contains("~"));
        assert!(readout.plain_text().starts_with("Throughput"));
    }

    #[test]
    fn wg5_exact_usage_drops_the_tilde_and_is_sticky() {
        let mut tracker = ThroughputTracker::new();
        // Starts approximate, then real usage lands mid-turn.
        tracker.observe(250, 10, false);
        tracker.observe(500, 30, false);
        assert!(tracker.readout().unwrap().approx);
        tracker.observe(750, 128, true); // exact arrives
        assert!(
            !tracker.readout().unwrap().approx,
            "exact seen → tilde gone for the turn"
        );
        // Sticky: a later approx feed does not re-flag the turn.
        tracker.observe(1_000, 150, false);
        assert!(!tracker.readout().unwrap().approx);
    }

    #[test]
    fn wg5_plain_text_omits_absent_aggregates() {
        let readout = ThroughputReadout {
            spark: "▁▂".to_owned(),
            tps: 126,
            approx: false,
            mean: None,
            p95: None,
        };
        assert_eq!(readout.plain_text(), "Throughput ▁▂ 126 tps");
        let full = ThroughputReadout {
            spark: "▁▂▃▄▅".to_owned(),
            tps: 126,
            approx: false,
            mean: Some(119),
            p95: Some(154),
        };
        assert_eq!(
            full.plain_text(),
            "Throughput ▁▂▃▄▅ 126 tps · μ 119 · p95 154"
        );
    }

    // ---- WG3 (unit half): the idle resting shape is empty + a no-op ----

    #[test]
    fn wg3_idle_reset_is_empty_and_reset_is_idempotent() {
        let mut tracker = ThroughputTracker::new();
        assert!(tracker.is_empty());
        assert_eq!(tracker.readout(), None);
        feed(&mut tracker, &[(250, 10, true), (500, 60, true)]);
        assert!(!tracker.is_empty());
        tracker.reset();
        assert!(tracker.is_empty(), "reset returns the idle shape");
        // A second reset changes nothing (idle ticks are no-ops → WG3).
        tracker.reset();
        assert!(tracker.is_empty());
        assert_eq!(tracker.readout(), None);
    }
}
