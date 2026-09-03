//! UI-polish wave pins: the `/model` picker's viewport-follow scroll (the `/`
//! palette rule), the synthesized listening wave animation, and the compact
//! throughput pill form.
#![allow(clippy::expect_used)]

use haider_tui::app::follow_viewport;
use haider_tui::talk::{LISTENING_SIGNAL_MIN, listening_pulse_cells};
use haider_tui::throughput::ThroughputReadout;

#[test]
fn viewport_follows_selection_without_scrolling_under_a_pinned_top() {
    // window of 5 over 20 rows. Moving the selection down the first window
    // does NOT scroll (top stays 0) — the highlight moves inside the window,
    // exactly the `/` palette behaviour, not a list scrolling on every key.
    let window = 5;
    let len = 20;
    let mut top = 0;
    for selection in 0..window {
        top = follow_viewport(top, selection, len, window);
        assert_eq!(top, 0, "no scroll while the selection is inside the window");
    }
    // Crossing the bottom edge advances the top by exactly one per step.
    top = follow_viewport(top, 5, len, window);
    assert_eq!(
        top, 1,
        "top follows only when the selection leaves the window"
    );
    top = follow_viewport(top, 6, len, window);
    assert_eq!(top, 2);
    // Moving back up above the top pulls it back minimally.
    top = follow_viewport(top, 1, len, window);
    assert_eq!(top, 1, "scrolling up follows the selection up, minimally");
    // The last page fills the window (top clamps at len - window).
    let bottom = follow_viewport(0, len - 1, len, window);
    assert_eq!(bottom, len - window, "the final page fills the window");
}

#[test]
fn viewport_is_a_noop_when_everything_fits() {
    assert_eq!(follow_viewport(0, 4, 5, 10), 0, "no scroll when list fits");
    assert_eq!(follow_viewport(3, 0, 0, 10), 0, "empty list clamps to zero");
}

#[test]
fn listening_pulse_animates_across_the_clock_and_is_deterministic() {
    // The synthesized sweep is a pure function of the wall clock: identical
    // input → identical cells (no randomness), but DIFFERENT clocks move the
    // crest, so the wave is visibly alive while listening.
    let a = listening_pulse_cells(0);
    let a_again = listening_pulse_cells(0);
    assert_eq!(a, a_again, "deterministic for a fixed clock");
    let b = listening_pulse_cells(400);
    let c = listening_pulse_cells(800);
    assert!(
        a != b || b != c,
        "the crest travels as the clock advances (the animation is alive)"
    );
    // Exactly one wave-width of cells, and the crest is HOT (gold) somewhere.
    assert_eq!(a.len(), haider_tui::talk::WAVE_WIDTH);
    assert!(
        b.iter().any(|cell| cell.hot),
        "the travelling crest lights a gold column"
    );
    // The threshold that routes real-vs-synthesized is a sane small value.
    let listening_signal_min = std::hint::black_box(LISTENING_SIGNAL_MIN);
    assert!(listening_signal_min > 0.0 && listening_signal_min < 0.2);
}

#[test]
fn throughput_pill_is_compact_and_carries_the_rate() {
    let readout = ThroughputReadout {
        spark: "▁▂▃▄▅▆".into(),
        tps: Some(126),
        elapsed_ms: 8_000,
        phase: haider_tui::throughput::ThroughputPhase::Live,
        approx: false,
        mean: Some(119),
        p95: Some(154),
    };
    let pill = readout.pill_text();
    // Compact: the rate + sparkline, but NOT the verbose "Throughput" label,
    // and (tpsfix 2026-09-03) neither μ nor p95 — the widget is a fixed
    // PILL_WIDTH strip and μ duplicates the settled headline number.
    assert!(pill.contains("126 tps"), "{pill}");
    assert!(pill.contains("▁▂▃▄▅▆"), "{pill}");
    assert!(
        !pill.contains('μ'),
        "μ dropped from the compact widget: {pill}"
    );
    assert!(!pill.contains("Throughput"), "no verbose label: {pill}");
    assert!(
        !pill.contains("p95"),
        "p95 dropped on the tight line: {pill}"
    );
    assert_eq!(
        pill.chars().count(),
        haider_tui::throughput::PILL_WIDTH,
        "the widget is a FIXED cell budget: {pill:?}"
    );
    // The approx `~` still rides an estimated rate, and costs no extra cell.
    let approx = ThroughputReadout {
        approx: true,
        ..readout
    };
    assert!(approx.pill_text().contains("~126 tps"));
    assert_eq!(
        approx.pill_text().chars().count(),
        haider_tui::throughput::PILL_WIDTH
    );
}
