//! tpsfix (v0.0.970) — the tokens-per-second estimator's laws, driven by
//! synthetic streams over mock time. Design note:
//! `docs/testing/v0.0.970/tpsfix.md`.
//!
//! The owner's bug had three shapes and each has a law here: `0 tps` while the
//! model thinks (warm-up), a `0 ↔ 5000` flap while it streams (the sub-window
//! and step-function guards), and a final figure that matched nothing (the
//! turn-mean settle).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tui::throughput::{
    DEFAULT_CHARS_PER_TOKEN, PILL_WIDTH, ThroughputPhase, ThroughputTracker,
};

/// The one turn epoch every single-turn law here runs under.
const TURN: u64 = 1;

/// Drive the tracker and return the value it DISPLAYED after each observation
/// — the sequence a user's eye actually sees, which is what "no flapping" is a
/// statement about.
fn displayed_series(script: &[(u64, u64, Option<u64>)]) -> (ThroughputTracker, Vec<Option<u32>>) {
    let mut tracker = ThroughputTracker::new();
    let mut seen = Vec::with_capacity(script.len());
    for &(at_ms, chars, exact) in script {
        tracker.observe(at_ms, TURN, chars, exact);
        seen.push(tracker.readout().and_then(|readout| readout.tps));
    }
    (tracker, seen)
}

/// A steady stream: `chars_per_ms * dt` characters land on every `step_ms`
/// tick, starting from an explicit zero-output observation at `t = 0` so the
/// generation clock has a floor to start from.
fn steady(step_ms: u64, chars_per_step: u64, steps: u64) -> Vec<(u64, u64, Option<u64>)> {
    let mut script = vec![(0, 0, None)];
    let mut chars = 0u64;
    for step in 1..=steps {
        chars += chars_per_step;
        script.push((step * step_ms, chars, None));
    }
    script
}

// ---- (a) the live window: a steady stream reads as its own rate ----

#[test]
fn steady_fifty_tokens_per_second_reads_as_fifty() {
    // 200 chars per second at the default 4 chars/token = 50 tok/s, sampled on
    // a 250ms tick for 20 seconds.
    let (tracker, seen) = displayed_series(&steady(250, 50, 80));
    let readout = tracker.readout().expect("a rate is established");
    assert_eq!(readout.phase, ThroughputPhase::Live);
    assert_eq!(readout.tps, Some(50), "a steady 50 tok/s reads as 50");
    // Every value the user saw was either warm-up or within tolerance — the
    // estimator never overshoots on the way up.
    for value in seen.iter().flatten() {
        assert!(
            (45..=55).contains(value),
            "a steady stream never leaves ±10%: {seen:?}"
        );
    }
    // And it never showed a zero while output was flowing.
    assert!(
        !seen.contains(&Some(0)),
        "no fabricated zero mid-stream: {seen:?}"
    );
}

#[test]
fn the_rate_holds_its_shape_across_a_slow_and_a_fast_stream() {
    // 10 tok/s and 400 tok/s over the same script shape: both land on their own
    // rate, so the estimator is not tuned to one speed.
    let (slow, _) = displayed_series(&steady(250, 10, 80));
    let (fast, _) = displayed_series(&steady(250, 400, 80));
    assert_eq!(slow.readout().unwrap().tps, Some(10));
    assert_eq!(fast.readout().unwrap().tps, Some(400));
}

// ---- (d) no flapping: the sub-window and burst guards ----

#[test]
fn a_burst_never_reads_as_its_naive_instantaneous_rate() {
    // 5 000 tokens (20 000 chars) inside 100ms, then silence. Differentiating
    // that pair directly gives 50 000 tps — the owner's `~5k` spike, scaled.
    let mut script = vec![(0, 0, None), (100, 20_000, None)];
    for step in 1..=40u64 {
        script.push((250 * step, 20_000, None));
    }
    let (tracker, seen) = displayed_series(&script);
    let values: Vec<u32> = seen.iter().flatten().copied().collect();
    let peak = values.iter().copied().max().expect("a rate is established");
    assert!(
        peak <= 10_000,
        "the 500ms floor caps the burst well under the naive 50 000: {peak}"
    );
    // Monotone decay after the peak — never a rebound, never a 0↔5k flap.
    let peak_at = values.iter().position(|value| *value == peak).unwrap();
    for pair in values[peak_at..].windows(2) {
        assert!(
            pair[1] <= pair[0],
            "the post-burst tail only falls: {values:?}"
        );
    }
    assert!(
        *values.last().unwrap() < 500,
        "ten seconds of silence decays the rate away: {values:?}"
    );
    assert!(
        values.iter().all(|value| *value > 0),
        "a live turn never wears a bare `0 tps`: {values:?}"
    );
    // The classic flap signature — a zero adjacent to a four-figure reading —
    // never appears.
    for pair in values.windows(2) {
        let flap = (pair[0] == 0 && pair[1] >= 1_000) || (pair[0] >= 1_000 && pair[1] == 0);
        assert!(!flap, "0 ↔ 5k flap at {pair:?} in {values:?}");
    }
    assert_eq!(tracker.readout().unwrap().phase, ThroughputPhase::Live);
}

#[test]
fn a_single_delta_inside_the_minimum_span_shows_nothing() {
    // One fat delta 200ms after the turn opened: the span is under MIN_SPAN_MS,
    // so there is no honest rate to show yet.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    tracker.observe(200, TURN, 20_000, None);
    let readout = tracker.readout().expect("the turn is open");
    assert_eq!(readout.tps, None, "no rate from a sub-500ms window");
    assert_eq!(readout.phase, ThroughputPhase::Warmup);
}

#[test]
fn a_sub_threshold_window_waits_for_the_window_to_age() {
    // 20 chars (5 tokens) is under MIN_WINDOW_TOKENS: withheld while the window
    // is young, then published once the window has fully aged, so a genuinely
    // slow stream still measures instead of freezing.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    tracker.observe(600, TURN, 20, None);
    assert_eq!(
        tracker.readout().unwrap().tps,
        None,
        "fewer than N tokens in a young window is not a rate"
    );
    tracker.observe(2_100, TURN, 20, None);
    assert_eq!(
        tracker.readout().unwrap().tps,
        Some(2),
        "an aged window publishes the honest slow rate"
    );
}

#[test]
fn the_displayed_value_is_recomputed_at_most_a_few_times_a_second() {
    // Deltas at 20ms cadence for two seconds: the EMIT_MS throttle means the
    // digits change far less often than the deltas arrive.
    let mut script = vec![(0, 0, None)];
    let mut chars = 0u64;
    for step in 1..=100u64 {
        chars += 4;
        script.push((20 * step, chars, None));
    }
    let (_, seen) = displayed_series(&script);
    let changes = seen
        .windows(2)
        .filter(|pair| pair[0] != pair[1] && pair[1].is_some())
        .count();
    assert!(
        changes <= 10,
        "at most ~4 recomputations per second over 2s: {changes} changes in {seen:?}"
    );
}

// ---- (c) thinking: elapsed time, never a fabricated zero ----

#[test]
fn silent_thinking_shows_elapsed_and_never_zero_tps() {
    let mut tracker = ThroughputTracker::new();
    for step in 0..=10u64 {
        tracker.observe(600 * step, TURN, 0, None);
        let readout = tracker.readout().expect("the turn is open");
        assert_eq!(readout.tps, None, "thinking never publishes a rate");
        assert_eq!(readout.phase, ThroughputPhase::Warmup);
    }
    let readout = tracker.readout().unwrap();
    assert_eq!(readout.elapsed_ms, 6_000);
    assert!(
        readout.plain_text().contains("thinking 6.0s"),
        "{}",
        readout.plain_text()
    );
    let field = readout.rate_field();
    assert!(field.contains('⋯') && !field.contains("tps"), "{field}");
    assert_eq!(readout.pill_text().chars().count(), PILL_WIDTH);
}

#[test]
fn billed_but_unstreamed_reasoning_stays_in_warmup_and_still_counts_at_the_end() {
    // The provider meters 900 output tokens of reasoning it never streamed.
    // There is no instantaneous rate to report — but the tokens are real, so
    // they must land in the turn total.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    for step in 1..=10u64 {
        tracker.observe(600 * step, TURN, 0, Some(900));
    }
    let readout = tracker.readout().unwrap();
    assert_eq!(readout.tps, None, "unstreamed tokens have no visible rate");
    assert!(!readout.approx, "an exact usage frame did land");
    // Then text streams at 200 chars/s (the model's real ratio is 4.0, but the
    // cumulative usage still carries the 900 unstreamed reasoning tokens). The
    // DELTA calibration isolates the streaming phase, so the rate converges on
    // the true 50 tok/s instead of inheriting the reasoning distortion.
    let mut chars = 0u64;
    for step in 1..=60u64 {
        chars += 50;
        tracker.observe(6_000 + 250 * step, TURN, chars, Some(900 + chars / 4));
    }
    assert!(
        (tracker.chars_per_token() - 4.0).abs() < 0.2,
        "the delta calibration recovers the real ratio: {}",
        tracker.chars_per_token()
    );
    let rate = tracker.readout().unwrap().tps.expect("text streams a rate");
    assert!((45..=55).contains(&rate), "the text phase measures: {rate}");
    // The unstreamed reasoning is still real output, so it lands in the total.
    tracker.settle(22_000, TURN, Some(900 + chars / 4));
    let settled = tracker.readout().unwrap().tps.expect("a settled rate");
    assert!(
        settled > 50,
        "the billed reasoning raises the turn mean above the text rate: {settled}"
    );
}

#[test]
fn reasoning_deltas_are_output_tokens_and_carry_the_rate() {
    // Reasoning content streams first, then assistant text, on one continuous
    // character counter: the rate is live through BOTH phases, which is the
    // 2026-08-15 owner bug (a flatline through thinking-heavy turns) staying
    // dead.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    let mut chars = 0u64;
    let mut reasoning_phase_rate = None;
    for step in 1..=12u64 {
        chars += 50;
        tracker.observe(250 * step, TURN, chars, None);
        if step == 12 {
            reasoning_phase_rate = tracker.readout().unwrap().tps;
        }
    }
    assert!(
        reasoning_phase_rate.is_some_and(|rate| (45..=55).contains(&rate)),
        "reasoning deltas carry the rate: {reasoning_phase_rate:?}"
    );
    for step in 13..=40u64 {
        chars += 50;
        tracker.observe(250 * step, TURN, chars, None);
    }
    assert_eq!(
        tracker.readout().unwrap().tps,
        Some(50),
        "the text phase continues the same generation clock"
    );
}

// ---- (b) usage-corrected totals: the calibrated ratio ----

#[test]
fn an_exact_usage_frame_recalibrates_the_ratio_without_a_step() {
    // A tokenizer that is really 2 chars/token (CJK, dense code): the default
    // 4.0 under-reports by half until the provider's first frame lands.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    let mut chars = 0u64;
    let mut seen = Vec::new();
    for step in 1..=20u64 {
        chars += 50;
        // The provider's cumulative total arrives at t = 5s and every 2s after.
        let exact = (step % 8 == 0).then_some(chars / 2);
        tracker.observe(250 * step, TURN, chars, exact);
        if let Some(rate) = tracker.readout().unwrap().tps {
            seen.push(rate);
        }
    }
    assert!(
        (tracker.chars_per_token() - 2.0).abs() < 1e-9,
        "the frame re-derived the ratio: {}",
        tracker.chars_per_token()
    );
    let first = seen.first().copied().expect("an early estimate");
    assert!(
        (45..=55).contains(&first),
        "the default 4.0 ratio reads 50 before calibration: {first}"
    );
    let last = seen.last().copied().unwrap();
    assert!(
        (90..=110).contains(&last),
        "the calibrated ratio reads the true 100: {last}"
    );
    // The correction lands on the RATIO, so the displayed series walks there —
    // no single step doubles.
    for pair in seen.windows(2) {
        assert!(
            u64::from(pair[1]) * 2 > u64::from(pair[0])
                && u64::from(pair[1]) < u64::from(pair[0]) * 2 + 4,
            "the rescale is smoothed, not a step: {seen:?}"
        );
    }
    assert!(!tracker.readout().unwrap().approx, "exact usage → no tilde");
}

#[test]
fn a_thin_usage_frame_never_moves_the_ratio() {
    // Under CALIBRATION_MIN_CHARS the frame is recorded (the `~` drops) but the
    // ratio is untouched: a frame billing tokens the provider never streamed
    // must not rewrite the ratio for the content that WAS streamed.
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    tracker.observe(500, TURN, 8, Some(4_000));
    assert!(
        (tracker.chars_per_token() - DEFAULT_CHARS_PER_TOKEN).abs() < 1e-9,
        "a thin frame leaves the ratio alone: {}",
        tracker.chars_per_token()
    );
    assert!(!tracker.readout().unwrap().approx);
}

#[test]
fn no_exact_frame_keeps_the_approximate_marker() {
    let (tracker, _) = displayed_series(&steady(250, 50, 40));
    let readout = tracker.readout().unwrap();
    assert!(readout.approx, "a byte-only estimate wears the ~");
    assert!(readout.plain_text().contains('~'));
    assert!(readout.rate_field().contains('~'));
    assert_eq!(readout.pill_text().chars().count(), PILL_WIDTH);
}

// ---- (e) the final value is the turn's own mean ----

#[test]
fn the_settled_value_is_total_output_over_the_generation_span() {
    let mut tracker = ThroughputTracker::new();
    // One second of thinking, then ten seconds of generation at 200 chars/s.
    tracker.observe(0, TURN, 0, None);
    tracker.observe(1_000, TURN, 0, None);
    let mut chars = 0u64;
    for step in 1..=10u64 {
        chars += 200;
        tracker.observe(1_000 + 1_000 * step, TURN, chars, None);
    }
    // Estimate only: 2 000 chars ÷ 4.0 = 500 tokens over 10s = 50 tps.
    tracker.settle(11_500, TURN, None);
    let estimated = tracker.readout().unwrap();
    assert_eq!(estimated.tps, Some(50));
    assert_eq!(estimated.phase, ThroughputPhase::Settled);
    assert_eq!(
        estimated.elapsed_ms, 10_000,
        "the settled elapsed is the GENERATION span, not the turn"
    );
    // The provider's own final figure then replaces the estimate: 600 tokens
    // over the same 10s span.
    tracker.settle(12_000, TURN, Some(600));
    assert_eq!(tracker.readout().unwrap().tps, Some(60));
    assert!(!tracker.readout().unwrap().approx);
}

#[test]
fn time_to_first_token_is_excluded_from_the_final_rate() {
    // The SAME generation, once after a 1s wait and once after a 30s wait: the
    // settled rate is identical, because the generation clock starts at the
    // first output token.
    let rate_after = |ttft_ms: u64| {
        let mut tracker = ThroughputTracker::new();
        tracker.observe(0, TURN, 0, None);
        tracker.observe(ttft_ms, TURN, 0, None);
        let mut chars = 0u64;
        for step in 1..=10u64 {
            chars += 200;
            tracker.observe(ttft_ms + 1_000 * step, TURN, chars, None);
        }
        tracker.settle(ttft_ms + 11_000, TURN, Some(500));
        tracker.readout().unwrap().tps
    };
    assert_eq!(rate_after(1_000), Some(50));
    assert_eq!(rate_after(30_000), rate_after(1_000));
}

#[test]
fn a_degenerate_generation_span_keeps_the_last_live_value() {
    // Everything arrived in one sub-MIN_SPAN_MS burst: dividing by that span
    // would publish a five-figure fiction, so the settle keeps what the live
    // window last measured (here: nothing at all).
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, TURN, 0, None);
    tracker.observe(100, TURN, 20_000, None);
    tracker.settle(200, TURN, Some(5_000));
    let readout = tracker.readout().unwrap();
    assert_eq!(readout.tps, None, "no fabricated final rate");
    assert_eq!(readout.phase, ThroughputPhase::Warmup);
}

// ---- turn scoping: the stale-usage root cause ----

#[test]
fn a_new_turn_epoch_starts_the_estimator_over() {
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, 1, 0, None);
    let mut chars = 0u64;
    for step in 1..=40u64 {
        chars += 50;
        tracker.observe(250 * step, 1, chars, Some(chars / 4));
    }
    tracker.settle(10_500, 1, Some(500));
    assert_eq!(tracker.readout().unwrap().tps, Some(50));
    assert!(!tracker.samples().is_empty());

    // Turn two opens. Nothing of turn one survives — not the rate, not the
    // sparkline, and not the exact total (the stale cumulative that used to be
    // read as the new turn's own, producing `0 tps` then a five-figure jump).
    tracker.observe(11_000, 2, 0, None);
    let fresh = tracker.readout().expect("turn two is open");
    assert_eq!(fresh.tps, None);
    assert_eq!(fresh.phase, ThroughputPhase::Warmup);
    assert!(fresh.approx, "turn two has seen no usage of its own");
    assert!(tracker.samples().is_empty());
}

#[test]
fn settling_a_turn_the_tracker_has_moved_past_is_a_no_op() {
    let mut tracker = ThroughputTracker::new();
    tracker.observe(0, 2, 0, None);
    let mut chars = 0u64;
    for step in 1..=20u64 {
        chars += 50;
        tracker.observe(250 * step, 2, chars, None);
    }
    let live = tracker.readout().unwrap();
    // A late terminal for turn ONE must not settle turn TWO.
    tracker.settle(6_000, 1, Some(9_999));
    let after = tracker.readout().unwrap();
    assert_eq!(after.tps, live.tps);
    assert_eq!(after.phase, ThroughputPhase::Live);
}

#[test]
fn reset_returns_the_idle_resting_shape() {
    let (mut tracker, _) = displayed_series(&steady(250, 50, 40));
    assert!(!tracker.is_empty());
    tracker.reset();
    assert!(tracker.is_empty());
    assert_eq!(tracker.readout(), None);
    tracker.reset();
    assert!(tracker.is_empty());
}

// ---- a whole realistic turn, end to end ----

#[test]
fn a_realistic_turn_never_flaps_between_zero_and_a_four_figure_rate() {
    // 3s of silent thinking, 20s of streaming at ~60 tok/s with the provider's
    // cumulative usage landing as a STEP every 4s, a 5s tool pause, then more
    // streaming — the exact shape that produced the owner's screenshot.
    let mut tracker = ThroughputTracker::new();
    let mut seen = Vec::new();
    let mut chars = 0u64;
    let push = |tracker: &ThroughputTracker, seen: &mut Vec<Option<u32>>| {
        seen.push(tracker.readout().and_then(|readout| readout.tps));
    };
    for step in 0..=5u64 {
        tracker.observe(600 * step, TURN, 0, None);
        push(&tracker, &mut seen);
    }
    let mut exact_total = 0u64;
    for step in 1..=80u64 {
        chars += 60;
        let at = 3_000 + 250 * step;
        if step % 16 == 0 {
            exact_total = chars / 4;
        }
        let exact = (exact_total > 0).then_some(exact_total);
        tracker.observe(at, TURN, chars, exact);
        push(&tracker, &mut seen);
    }
    // A five-second tool pause: no growth, no new characters.
    for step in 1..=20u64 {
        tracker.observe(23_000 + 250 * step, TURN, chars, Some(exact_total));
        push(&tracker, &mut seen);
    }
    for step in 1..=40u64 {
        chars += 60;
        tracker.observe(28_000 + 250 * step, TURN, chars, Some(exact_total));
        push(&tracker, &mut seen);
    }
    let values: Vec<u32> = seen.iter().flatten().copied().collect();
    for pair in values.windows(2) {
        let flap = (pair[0] == 0 && pair[1] >= 1_000) || (pair[0] >= 1_000 && pair[1] == 0);
        assert!(!flap, "0 ↔ 4-figure flap at {pair:?}");
        assert!(
            pair[1] <= pair[0].max(1) * 3,
            "no single step triples the reading: {pair:?}"
        );
    }
    assert!(
        values.iter().all(|value| *value < 1_000),
        "a 60 tok/s turn never reads four figures: max {:?}",
        values.iter().max()
    );
    // The warm-up beats published nothing at all.
    assert!(
        seen[..6].iter().all(Option::is_none),
        "thinking published no rate: {:?}",
        &seen[..6]
    );
    tracker.settle(39_000, TURN, Some(chars / 4));
    let final_readout = tracker.readout().unwrap();
    let final_tps = final_readout.tps.expect("a settled rate");
    // 1 800 tokens over the 33s generation span (the tool pause is inside it).
    assert!(
        (50..=60).contains(&final_tps),
        "the settled figure is the turn's own mean: {final_tps}"
    );
    assert_eq!(final_readout.phase, ThroughputPhase::Settled);
}
