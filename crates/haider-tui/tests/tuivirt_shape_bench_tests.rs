//! tuivirt shape gate — the replacement for optimization-ledger row 17's
//! benchmark (`w3c3_render_bench_tests`), stated as the re-architecture's
//! TARGET shape rather than today's calibrated bounds:
//!
//! * the FIRST frame of a freshly attached session is ≤ 33 ms at 10k, 50k
//!   and 200k rows (no O(N) cold cache fill);
//! * the cached p95 stays ≤ 33 ms at every size, following and mid-scroll;
//! * both are FLAT: the 200k figure is within 20 % of the 10k figure.
//!
//! ONLY AN OPTIMIZED BUILD MEASURES THE THRESHOLDS (the row-17 bench's own
//! discipline): a debug build prints a loud SKIP and asserts nothing.
//!
//! ```text
//! cargo test --release -p haider-tui --test tuivirt_shape_bench_tests -- --nocapture
//! ```
//!
//! This is ship-gate ledger row 17. It is always enabled; debug builds print
//! a loud timing SKIP, while the release profile enforces the shape.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tui::app::AppModel;
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

mod tuivirt_common;
use tuivirt_common::replayed;

const FRAME_BUDGET: Duration = Duration::from_millis(33);
/// Flatness: the largest size may exceed the smallest by at most 20 %,
/// plus a small absolute allowance so sub-millisecond jitter cannot fail
/// a genuinely flat curve.
const FLAT_RATIO: f64 = 1.20;
const FLAT_SLACK: Duration = Duration::from_millis(1);
const SIZES: [usize; 3] = [10_000, 50_000, 200_000];

fn samples() -> usize {
    if cfg!(debug_assertions) { 5 } else { 60 }
}

fn one_frame(model: &AppModel, terminal: &mut Terminal<TestBackend>) -> Duration {
    let start = Instant::now();
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw succeeds");
    start.elapsed()
}

fn frame_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn percentile(mut timings: Vec<Duration>, percentile: usize) -> Duration {
    timings.sort_unstable();
    timings[(timings.len() * percentile / 100).min(timings.len() - 1)]
}

struct Shape {
    rows: usize,
    first: Duration,
    p95_follow: Duration,
    p95_middle: Duration,
}

fn measure(rows: usize) -> Shape {
    // Model and terminal construction stay outside every interval.
    let model = replayed(rows);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    let first = one_frame(&model, &mut terminal);
    let first_frame_text = frame_text(&terminal);
    assert!(
        first_frame_text.contains(&format!("row {}", rows - 1)),
        "the first frame must open on the real visible tail at {rows} rows"
    );
    let follow: Vec<Duration> = (0..samples())
        .map(|_| one_frame(&model, &mut terminal))
        .collect();
    model.scroll_back.set(model.scroll_max.get() / 2);
    let middle: Vec<Duration> = (0..samples())
        .map(|_| one_frame(&model, &mut terminal))
        .collect();
    model.scroll_back.set(model.scroll_max.get());
    let _ = one_frame(&model, &mut terminal);
    assert!(
        frame_text(&terminal).contains("row 0 —"),
        "the full {rows}-row coordinate space must remain navigable past u16::MAX"
    );
    Shape {
        rows,
        first,
        p95_follow: percentile(follow, 95),
        p95_middle: percentile(middle, 95),
    }
}

fn flat(what: &str, small: Duration, large: Duration) {
    let ceiling = small.mul_f64(FLAT_RATIO) + FLAT_SLACK;
    assert!(
        large <= ceiling,
        "{what} must stay flat from 10k to 200k rows: 10k={small:?} 200k={large:?} (ceiling {ceiling:?})"
    );
}

#[test]
fn first_frame_and_cached_p95_are_flat_from_10k_to_200k_rows() {
    // Warm the allocator/terminal once so the first sample is not the
    // benchmark.
    let _ = measure(64);
    let shapes: Vec<Shape> = SIZES.into_iter().map(measure).collect();
    for shape in &shapes {
        println!(
            "tuivirt shape @ {} rows: first={:?} p95(follow)={:?} p95(middle)={:?}",
            shape.rows, shape.first, shape.p95_follow, shape.p95_middle
        );
    }
    if cfg!(debug_assertions) {
        println!("tuivirt shape gate = SKIP (unoptimized build). Run with --release to enforce.");
        return;
    }
    for shape in &shapes {
        assert!(
            shape.first <= FRAME_BUDGET,
            "first frame @ {} rows must fit the 33 ms budget: {:?}",
            shape.rows,
            shape.first
        );
        assert!(
            shape.p95_follow <= FRAME_BUDGET,
            "cached following p95 @ {} rows must fit the 33 ms budget: {:?}",
            shape.rows,
            shape.p95_follow
        );
        assert!(
            shape.p95_middle <= FRAME_BUDGET,
            "cached mid-scroll p95 @ {} rows must fit the 33 ms budget: {:?}",
            shape.rows,
            shape.p95_middle
        );
    }
    let (small, large) = (&shapes[0], &shapes[2]);
    flat("first frame", small.first, large.first);
    flat("cached following p95", small.p95_follow, large.p95_follow);
    flat("cached mid-scroll p95", small.p95_middle, large.p95_middle);
}

/// MUTATION CHECK for the gate's own arithmetic, always on: the flatness
/// ceiling is 1.2× + 1 ms, and the percentile picks the right sample.
#[test]
fn shape_gate_arithmetic_is_pinned() {
    flat(
        "exactly flat",
        Duration::from_millis(10),
        Duration::from_millis(10),
    );
    flat(
        "within 20 %",
        Duration::from_millis(10),
        Duration::from_millis(12),
    );
    flat(
        "slack covers jitter",
        Duration::from_micros(500),
        Duration::from_micros(1500),
    );
    let result = std::panic::catch_unwind(|| {
        flat(
            "too steep",
            Duration::from_millis(10),
            Duration::from_millis(14),
        );
    });
    assert!(result.is_err(), "a 40 % rise must fail the flatness gate");
    let timings = (1..=100u64).map(Duration::from_millis).collect::<Vec<_>>();
    assert_eq!(percentile(timings.clone(), 95), Duration::from_millis(96));
    assert_eq!(percentile(timings, 50), Duration::from_millis(51));
}
