//! W-G live token-throughput estimator: a PURE sampler that turns the streamed
//! output signal into the compact status widget — `▁▂▃▄▅▆  126 tps` — carried
//! above the composer.
//!
//! DEFINITION (v0.0.970 `tpsfix`, design note
//! `docs/testing/v0.0.970/tpsfix.md`):
//!
//! - An OUTPUT token is a token the model generated this turn: assistant text,
//!   reasoning/thinking content, and tool-call argument fragments. Tool results
//!   and command output are execution data, not generation. This is exactly the
//!   set the providers meter as `output_tokens` / `completion_tokens` (both of
//!   which already include reasoning tokens), so `Usage::output` is used as-is
//!   and `Usage::reasoning` is never added to it.
//! - The GENERATION CLOCK starts at the first output token, not at turn start:
//!   time-to-first-token is excluded, exactly as llama.cpp separates
//!   `prompt eval time` from `eval time` and ollama separates
//!   `prompt_eval_count/duration` from `eval_count/duration`.
//! - The LIVE rate is a [`WINDOW_MS`] sliding window, EMA-smoothed
//!   ([`EMA_ALPHA`]) and emitted at most every [`EMIT_MS`]. It is never
//!   computed over a span shorter than [`MIN_SPAN_MS`] or (before the window
//!   has fully aged) from fewer than [`MIN_WINDOW_TOKENS`] tokens — the two
//!   guards that killed the owner's `0 ↔ 5000` flap.
//! - The FINAL rate at turn end is total output tokens ÷ (generation start →
//!   last growth), using the provider's own final figure when it reported one.
//!
//! TOKEN COUNTING (the anti-flap core). Provider usage arrives as a STEP
//! function — one cumulative frame per physical provider request — so
//! differentiating it directly is what produced the flap. This tracker instead
//! takes the SHAPE from the streamed-character counter (which advances on every
//! delta and is therefore smooth) and the SCALE from one calibrated ratio,
//! `chars_per_token`: it starts at [`DEFAULT_CHARS_PER_TOKEN`] and is
//! re-derived from every exact usage frame as `chars ÷ usage.output`, clamped
//! to `[CPT_MIN, CPT_MAX]`. At the instant a usage frame lands,
//! `chars ÷ chars_per_token` equals the provider's figure EXACTLY — the real
//! number replaces the estimate without a step, because the correction lands on
//! the ratio, not on the series.
//!
//! HONESTY: a readout that has never seen an exact usage frame wears a leading
//! `~`. A turn that has produced no output token yet reports
//! [`ThroughputPhase::Warmup`] and its elapsed time — never a fabricated
//! `0 tps`.

use std::collections::VecDeque;

/// The unicode block ramp, low → high, that maps a sample's magnitude to a
/// sparkline column.
pub const SPARK_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Per-turn tps samples kept for the sparkline + aggregate stats.
const SAMPLE_CAP: usize = 6;
/// The sparkline's FIXED rendered width. Owner 2026-09-03: the widget is a
/// small fixed-width strip (about a quarter of its former ~40 columns), NOT a
/// fraction of the terminal — it must not grow with the frame. Six columns at
/// one closed [`BUCKET_MS`] bucket each is 30s of visible history.
pub const SPARK_WIDTH: usize = SAMPLE_CAP;
/// Columns reserved for the rate digits (right-aligned; an approximate
/// readout's `~` eats one of them), so the ` tps` unit never shifts.
pub const RATE_WIDTH: usize = 4;
/// The widget's TOTAL fixed cell budget: sparkline + a gap + the rate field
/// (digits + ` tps`). Fifteen cells, plus the row's one-cell left margin.
pub const PILL_WIDTH: usize = SPARK_WIDTH + 1 + RATE_WIDTH + 4;

/// Raw `(t_ms, cumulative_output_chars)` observations retained for the
/// windowed rate. Bounded well above one window's worth at delta cadence.
const RAW_CAP: usize = 256;
/// The sliding window (ms) the instantaneous rate is measured over.
pub const WINDOW_MS: u64 = 2_000;
/// A rate is never computed over a shorter span than this — the guard that
/// stops one fat delta from reading as tens of thousands of tps.
pub const MIN_SPAN_MS: u64 = 500;
/// A rate is never computed from fewer tokens than this UNLESS the window has
/// fully aged (`span >= WINDOW_MS`), in which case a genuinely slow or stopped
/// stream is allowed to decay the EMA instead of freezing it. With the 2s
/// window this puts the measurable floor at 4 tok/s.
pub const MIN_WINDOW_TOKENS: f64 = 8.0;
/// The displayed value is recomputed at most this often, so the digits do not
/// strobe at delta cadence.
pub const EMIT_MS: u64 = 250;
/// Exponential-moving-average weight on each emitted sample.
pub const EMA_ALPHA: f64 = 0.4;
/// One sparkline column is one closed BUCKET of this many milliseconds — the
/// bar is the turn's AVERAGE rate across its bucket.
const BUCKET_MS: u64 = 5_000;
/// Samples required before μ/p95 are shown on the verbose plain row.
const STATS_MIN: usize = 4;
/// The starting characters-per-token ratio before any exact usage frame — the
/// usual English-text ratio for BPE vocabularies.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 4.0;
/// Calibration clamp: below this a ratio would inflate the rate absurdly.
const CPT_MIN: f64 = 1.5;
/// Calibration clamp: above this a ratio would flatten the rate absurdly.
const CPT_MAX: f64 = 12.0;
/// Characters required before an exact usage frame is trusted to calibrate the
/// ratio — a frame that bills tokens the provider never streamed must not
/// rewrite the ratio for the content that WAS streamed.
const CALIBRATION_MIN_CHARS: u64 = 64;
/// The widest rate the fixed [`RATE_WIDTH`] field can carry.
const RATE_MAX: u32 = 9_999;

/// Where the turn stands, so the widget can show elapsed time instead of a
/// fabricated zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThroughputPhase {
    /// A turn is live but no measurable output has arrived yet: show elapsed
    /// time. NEVER `0 tps`.
    Warmup,
    /// A measured, live windowed rate.
    Live,
    /// The turn ended; the rate is the turn's own mean.
    Settled,
}

/// The pure, styleable readout — the render and plain layers both build their
/// line from these fields, so the two never drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThroughputReadout {
    /// The sparkline over the current bucket ring, padded to [`SPARK_WIDTH`].
    pub spark: String,
    /// The displayed rate, or `None` in [`ThroughputPhase::Warmup`].
    pub tps: Option<u32>,
    /// Generation elapsed (or, in warm-up, turn elapsed) in milliseconds.
    pub elapsed_ms: u64,
    /// Where the turn stands.
    pub phase: ThroughputPhase,
    /// True while the rate rests on the calibrated byte estimate — no exact
    /// usage frame has landed this turn. Rendered as a leading `~`.
    pub approx: bool,
    /// Mean of the closed 5s buckets, once `STATS_MIN` exist. A distribution
    /// figure for the verbose `--plain` row only: the compact widget dropped
    /// `μ` because at turn end it duplicates the headline number.
    pub mean: Option<u32>,
    /// 95th percentile of the closed 5s buckets, once `STATS_MIN` exist.
    pub p95: Option<u32>,
}

impl ThroughputReadout {
    /// The part of the compact widget that follows the sparkline, padded to a
    /// FIXED width so nothing to its right ever shifts. The render layer styles
    /// this separately from the spark; [`Self::pill_text`] is the same glyphs
    /// unstyled.
    #[must_use]
    pub fn rate_field(&self) -> String {
        match self.tps {
            Some(tps) => {
                // The `~` rides the digits as ONE right-aligned token, so an
                // estimate reads `~50` and not `~ 50`, and the marker costs a
                // digit column instead of a cell.
                let (tilde, cap) = if self.approx {
                    ("~", RATE_MAX / 10)
                } else {
                    ("", RATE_MAX)
                };
                let value = format!("{tilde}{}", tps.min(cap));
                format!(" {value:>RATE_WIDTH$} tps")
            }
            None => {
                let body = format!("⋯ {}", format_elapsed(self.elapsed_ms));
                let width = PILL_WIDTH - SPARK_WIDTH - 1;
                format!(" {body:>width$}")
            }
        }
    }

    /// Compact form for the status row — the fixed-width widget the owner sees.
    /// Exactly [`PILL_WIDTH`] cells. The `tps` token anchors it for parity
    /// greps.
    #[must_use]
    pub fn pill_text(&self) -> String {
        format!("{}{}", self.spark, self.rate_field())
    }

    /// The verbose plain-mode line — the parity anchor for `--plain` and the CI
    /// oracle. This surface keeps `μ`/`p95` (the closed-bucket distribution);
    /// the compact widget does not.
    #[must_use]
    pub fn plain_text(&self) -> String {
        let Some(tps) = self.tps else {
            return format!(
                "Throughput {} thinking {}",
                self.spark,
                format_elapsed(self.elapsed_ms)
            );
        };
        let tilde = if self.approx { "~" } else { "" };
        let mut line = format!("Throughput {} {tilde}{tps} tps", self.spark);
        if let Some(mean) = self.mean {
            line.push_str(&format!(" · μ {mean}"));
        }
        if let Some(p95) = self.p95 {
            line.push_str(&format!(" · p95 {p95}"));
        }
        line
    }
}

/// A compact elapsed rendering, at most five cells: `3.2s`, `42s`, `1m02`.
#[must_use]
pub fn format_elapsed(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else {
        let mins = (ms / 60_000).min(99);
        let secs = (ms % 60_000) / 1_000;
        format!("{mins}m{secs:02}")
    }
}

/// The per-turn estimator. Fed `(now_ms, turn, cumulative_output_chars,
/// exact_output_tokens)` on the existing frame clock — there is NO timer of its
/// own — and settled when the turn leaves the live set.
#[derive(Debug, Clone)]
pub struct ThroughputTracker {
    /// The turn epoch this state belongs to. A different epoch starts over.
    turn: Option<u64>,
    /// When this turn's first observation landed.
    turn_open_ms: u64,
    /// The newest observation time seen (monotone).
    now_ms: u64,
    /// The last observation that still had ZERO output — the tightest visible
    /// upper bound on time-to-first-token, and so the generation clock's zero.
    zero_ms: Option<u64>,
    /// Generation start; `None` until the first output character appears.
    gen_start_ms: Option<u64>,
    /// The last observation at which the output count grew.
    last_growth_ms: u64,
    /// Cumulative streamed output characters (monotone within the turn).
    chars: u64,
    /// The calibrated characters-per-token ratio.
    cpt: f64,
    /// Sticky: an exact usage frame landed this turn, so the `~` is dropped.
    exact_seen: bool,
    /// The provider's latest exact output-token total FOR THIS TURN.
    exact_total: Option<u64>,
    /// The `(chars, exact_tokens)` anchor the next calibration measures FROM.
    calib_chars: u64,
    /// See [`Self::calib_chars`].
    calib_exact: u64,
    /// `(t_ms, cumulative_chars)` observations spanning the rate window.
    raw: VecDeque<(u64, u64)>,
    /// The smoothed rate in tokens/second.
    ema: Option<f64>,
    /// When the displayed value was last recomputed.
    last_emit_ms: u64,
    /// The value the widget shows; `None` in warm-up.
    displayed: Option<u32>,
    /// The turn ended and `displayed` is its mean.
    settled: bool,
    /// Closed 5s bucket averages for the sparkline + plain stats.
    samples: VecDeque<u32>,
    /// The open bucket's `(start_ms, chars_at_start)`.
    bucket_start: Option<(u64, u64)>,
}

impl Default for ThroughputTracker {
    fn default() -> Self {
        Self {
            turn: None,
            turn_open_ms: 0,
            now_ms: 0,
            zero_ms: None,
            gen_start_ms: None,
            last_growth_ms: 0,
            chars: 0,
            cpt: DEFAULT_CHARS_PER_TOKEN,
            exact_seen: false,
            exact_total: None,
            calib_chars: 0,
            calib_exact: 0,
            raw: VecDeque::new(),
            ema: None,
            last_emit_ms: 0,
            displayed: None,
            settled: false,
            samples: VecDeque::new(),
            bucket_start: None,
        }
    }
}

impl ThroughputTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the tracker holds no state — the idle resting shape, in which
    /// further idle ticks are no-ops so idle frames stay byte-identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turn.is_none() && self.displayed.is_none() && self.samples.is_empty()
    }

    /// Clear all state (a fresh session).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Start a fresh turn, keeping nothing from the last one.
    fn begin(&mut self, turn: u64, now_ms: u64) {
        *self = Self {
            turn: Some(turn),
            turn_open_ms: now_ms,
            now_ms,
            last_growth_ms: now_ms,
            ..Self::default()
        };
    }

    /// Fold one exact usage frame into the calibration + total.
    ///
    /// The ratio is re-derived from the DELTA since the last calibration
    /// anchor, not from the absolute totals: a turn whose first frame also
    /// bills reasoning the provider never streamed would otherwise crush the
    /// ratio for the text that WAS streamed, and every later frame would
    /// inherit the distortion. Measuring the delta isolates the phase actually
    /// being metered, so the ratio converges on the model's real tokenizer
    /// within a frame or two. Only a delta backed by enough streamed
    /// characters is trusted to move it.
    fn absorb_exact(&mut self, exact: Option<u64>) {
        let Some(total) = exact else { return };
        self.exact_seen = true;
        self.exact_total = Some(total);
        let delta_chars = self.chars.saturating_sub(self.calib_chars);
        let delta_tokens = total.saturating_sub(self.calib_exact);
        if delta_tokens > 0 && delta_chars >= CALIBRATION_MIN_CHARS {
            self.cpt = (delta_chars as f64 / delta_tokens as f64).clamp(CPT_MIN, CPT_MAX);
            self.calib_chars = self.chars;
            self.calib_exact = total;
        }
    }

    /// Record one observation of this turn's cumulative streamed output
    /// characters and (when the provider reported one) its exact output-token
    /// total. A different `turn` epoch starts the turn over — the tracker never
    /// carries a stale cumulative across turns, which is what made the previous
    /// implementation read the PREVIOUS turn's usage as this turn's total.
    pub fn observe(&mut self, now_ms: u64, turn: u64, chars: u64, exact: Option<u64>) {
        if self.turn != Some(turn) {
            self.begin(turn, now_ms);
        }
        self.now_ms = self.now_ms.max(now_ms);
        let now_ms = self.now_ms;
        self.settled = false;

        if chars > self.chars {
            if self.gen_start_ms.is_none() {
                self.gen_start_ms = Some(self.zero_ms.unwrap_or(self.turn_open_ms));
            }
            self.last_growth_ms = now_ms;
            self.chars = chars;
        } else if self.chars == 0 {
            self.zero_ms = Some(now_ms);
        }
        // Calibration reads `self.chars`, so it lands AFTER the growth fold.
        self.absorb_exact(exact);

        self.raw.push_back((now_ms, self.chars));
        while self.raw.len() > RAW_CAP {
            self.raw.pop_front();
        }
        // Keep exactly one observation older than the window so the window is
        // fully spanned rather than truncated.
        while self.raw.len() > 2 && now_ms.saturating_sub(self.raw[1].0) >= WINDOW_MS {
            self.raw.pop_front();
        }

        self.update_rate(now_ms);
        self.roll_buckets(now_ms);
    }

    /// The turn left the live set: replace the live number with the turn's own
    /// mean — total output tokens ÷ (generation start → last growth) — using
    /// the provider's final figure when it reported one. Idempotent, and a
    /// usage frame that lands AFTER the terminal state refines it.
    pub fn settle(&mut self, now_ms: u64, turn: u64, exact: Option<u64>) {
        if self.turn != Some(turn) {
            return;
        }
        self.now_ms = self.now_ms.max(now_ms);
        self.absorb_exact(exact);
        self.settled = true;
        let Some(start) = self.gen_start_ms else {
            return;
        };
        let span = self.last_growth_ms.saturating_sub(start);
        // A degenerate span would divide the whole turn by one tick; keep the
        // last live value instead of inventing a five-figure rate.
        if span < MIN_SPAN_MS {
            return;
        }
        let rate = self.total_tokens() * 1_000.0 / span as f64;
        self.ema = Some(rate);
        self.displayed = Some(round_rate(rate));
    }

    /// This turn's best-known output-token total: the provider's figure when it
    /// reported one, otherwise the calibrated byte estimate.
    fn total_tokens(&self) -> f64 {
        match self.exact_total {
            Some(total) if total > 0 => total as f64,
            _ => self.chars as f64 / self.cpt,
        }
    }

    /// The windowed rate in tokens/second, or `None` when the window is too
    /// short or (before it has aged) too sparse to measure honestly.
    fn window_rate(&self) -> Option<f64> {
        // GENERATION has to have started. Without this the aged-window escape
        // below would publish `0 tps` after two seconds of silent thinking —
        // the owner's screenshot, exactly.
        self.gen_start_ms?;
        let &(t0, c0) = self.raw.front()?;
        let &(t1, c1) = self.raw.back()?;
        let span = t1.checked_sub(t0)?;
        if span < MIN_SPAN_MS {
            return None;
        }
        let tokens = (c1.saturating_sub(c0)) as f64 / self.cpt;
        if tokens < MIN_WINDOW_TOKENS && span < WINDOW_MS {
            return None;
        }
        Some(tokens * 1_000.0 / span as f64)
    }

    /// Recompute the displayed value at most every [`EMIT_MS`]; an
    /// unmeasurable window HOLDS the previous value rather than flapping to
    /// zero.
    fn update_rate(&mut self, now_ms: u64) {
        if self.displayed.is_some() && now_ms.saturating_sub(self.last_emit_ms) < EMIT_MS {
            return;
        }
        let Some(rate) = self.window_rate() else {
            return;
        };
        self.last_emit_ms = now_ms;
        let next = match self.ema {
            None => rate,
            Some(prev) => EMA_ALPHA.mul_add(rate - prev, prev),
        };
        self.ema = Some(next);
        // Floored at 1: a stalled stream decays toward zero, but a LIVE turn
        // never wears the `0 tps` the owner reported. Zero is reserved for
        // "not generating", which the warm-up form says in words.
        self.displayed = Some(round_rate(next).max(1));
    }

    /// Close every fully elapsed 5s bucket at its own average rate. The ring
    /// opens at GENERATION start, so the sparkline is a picture of generation
    /// and not of the thinking that preceded it.
    fn roll_buckets(&mut self, now_ms: u64) {
        let Some(gen_start) = self.gen_start_ms else {
            return;
        };
        let (start_ms, start_chars) = *self.bucket_start.get_or_insert((gen_start, 0));
        let elapsed = now_ms.saturating_sub(start_ms);
        if elapsed < BUCKET_MS {
            return;
        }
        let delta_chars = self.chars.saturating_sub(start_chars);
        let average = round_rate(delta_chars as f64 / self.cpt * 1_000.0 / elapsed as f64);
        let full = elapsed / BUCKET_MS;
        for _ in 0..full {
            self.samples.push_back(average);
            while self.samples.len() > SAMPLE_CAP {
                self.samples.pop_front();
            }
        }
        let consumed_ms = full * BUCKET_MS;
        let consumed_chars = delta_chars.saturating_mul(consumed_ms) / elapsed;
        self.bucket_start = Some((start_ms + consumed_ms, start_chars + consumed_chars));
    }

    /// The current readout, or `None` before any turn has been observed.
    #[must_use]
    pub fn readout(&self) -> Option<ThroughputReadout> {
        self.turn?;
        let ring: Vec<u32> = self.samples.iter().copied().collect();
        let enough = ring.len() >= STATS_MIN;
        // Fixed-width roll, LEFT-anchored: the box is SPARK_WIDTH columns from
        // the first frame, bars fill left-to-right as buckets close, and once
        // the ring caps the oldest bar falls off the left. Unfilled columns
        // stay blank — never a fabricated floor glyph.
        let drawn = spark(&ring, SPARK_WIDTH);
        let pad = SPARK_WIDTH.saturating_sub(drawn.chars().count());
        let spark = format!("{drawn}{}", " ".repeat(pad));
        let phase = match (self.displayed, self.settled) {
            (None, _) => ThroughputPhase::Warmup,
            (Some(_), true) => ThroughputPhase::Settled,
            (Some(_), false) => ThroughputPhase::Live,
        };
        let elapsed_ms = match self.gen_start_ms {
            Some(start) if self.settled => self.last_growth_ms.saturating_sub(start),
            Some(start) => self.now_ms.saturating_sub(start),
            None => self.now_ms.saturating_sub(self.turn_open_ms),
        };
        Some(ThroughputReadout {
            spark,
            tps: self.displayed,
            elapsed_ms,
            phase,
            approx: !self.exact_seen,
            mean: enough.then(|| mean(&ring)).flatten(),
            p95: enough.then(|| percentile(&ring, 95)).flatten(),
        })
    }

    /// The closed-bucket ring, oldest first — the sparkline's own data, and
    /// the assertion surface the estimator tests read.
    #[must_use]
    pub fn samples(&self) -> Vec<u32> {
        self.samples.iter().copied().collect()
    }

    /// The calibrated characters-per-token ratio currently in force.
    #[must_use]
    pub const fn chars_per_token(&self) -> f64 {
        self.cpt
    }
}

/// Round a tokens/second figure to the displayed integer, saturating rather
/// than wrapping and never producing a negative or NaN column.
#[must_use]
fn round_rate(rate: f64) -> u32 {
    if !rate.is_finite() || rate <= 0.0 {
        return 0;
    }
    let rounded = rate.round();
    if rounded >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        rounded as u32
    }
}

/// Map a sample buffer to a sparkline of at most `width` columns (the last
/// `width` samples), each column scaled across the window's own [min, max]
/// range so a rising series ramps monotonically and a flat series renders
/// flat-low. Empty in → empty out; never panics.
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

/// The integer mean of the samples (rounded), or `None` when empty.
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
/// rank `⌈p/100 · n⌉` (1-based), clamped into range. On a 1..=100 distribution
/// `percentile(_, 95) == 95`.
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
