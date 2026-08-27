//! Long-transcript render performance law.
//!
//! The earlier W3c3 probe established the trigger for cached entry layout and
//! viewport-only selection. The speed-optimization wave implements it, so this
//! probe now protects the shipped property: after the cold cache fill, a 10k
//! transcript must remain inside the live 33 ms frame budget and must not
//! scale linearly with history length.
//!
//! Timing is noisy on a shared machine, so the enforced timing assertions
//! are deliberately coarse (order-of-magnitude regression guards, not a
//! stopwatch); the printed p95s are the evidence a reviewer reads.
//!
//! ONLY AN OPTIMIZED BUILD MEASURES THE THRESHOLD. The shipped warm viewport
//! is sub-millisecond; an unoptimized build is much slower and comparing it
//! to release evidence would either fire the trigger falsely
//! forever or force a debug-shaped threshold that means nothing. So the
//! ledger gate runs under `--release` and prints a loud SKIP otherwise —
//! the probe ladder's own discipline: a bypassed check is announced, never
//! silently folded into a pass. Run it with:
//!
//! ```text
//! cargo test --release -p haider-tui --test w3c3_render_bench_tests -- --nocapture
//! ```
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::ids::ItemId;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::app::{AppModel, Screen};
use haider_tui::render::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

mod common;
use common::launcher_model;

const FRAME_BUDGET: Duration = Duration::from_millis(33);
const SHIPPED_MARKER: &str = "SHIPPED speed-opt";

/// A model whose attached session holds `rows` transcript rows, built the
/// way a REPLAY builds them: committed item envelopes through the reducer.
fn replayed(rows: usize) -> AppModel {
    let mut model = launcher_model();
    let session = model.sessions[0].id.clone();
    model.open_session(&session);
    for n in 0..rows {
        model
            .projection
            .apply(&EventPayload::Item(ItemEvent::Completed {
                item_id: ItemId::new(format!("bench-{n}")),
                item: TurnItem::AgentMessage {
                    text: format!(
                        "row {n} — a representative agent line with enough words to wrap at \
                         a normal terminal width and exercise the measurement path"
                    ),
                },
            }));
    }
    model.screen = Screen::Session;
    model
}

/// Samples per size. A debug build is ~15x slower and is only ever
/// informational here, so it takes far fewer.
fn samples() -> usize {
    if cfg!(debug_assertions) { 8 } else { 60 }
}

/// p95 of `samples` full frames at 118x36.
fn p95_frame(model: &AppModel, samples: usize) -> Duration {
    let backend = TestBackend::new(118, 36);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        terminal
            .draw(|frame| {
                render(model, frame);
            })
            .expect("draw succeeds");
        timings.push(start.elapsed());
    }
    timings.sort_unstable();
    timings[(samples * 95 / 100).min(samples - 1)]
}

/// Measures the first draw of a newly constructed 10k-row model. Model and
/// terminal construction stay outside the interval, matching the original
/// cold-cache measurement.
fn cold_10k_frame() -> Duration {
    let model = replayed(10_000);
    let backend = TestBackend::new(118, 36);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let start = Instant::now();
    terminal
        .draw(|frame| {
            render(&model, frame);
        })
        .expect("draw");
    start.elapsed()
}

fn minimum_duration(samples: &[Duration]) -> Duration {
    *samples.iter().min().expect("at least one cold sample")
}

#[test]
fn cached_viewport_render_stays_bounded_through_10k_rows() {
    // Warm the allocator/terminal once so the first sample is not the
    // benchmark.
    let _ = p95_frame(&replayed(64), 3);

    let mut table = Vec::new();
    for rows in [1_000_usize, 3_000, 5_000, 10_000] {
        let model = replayed(rows);
        let p95 = p95_frame(&model, samples());
        println!("render p95 @ {rows} rows = {p95:?}");
        table.push((rows, p95));
    }

    // The scroll-back render is bounded by the viewport, not history.
    let (_, p95_1k) = table[0];
    let (_, p95_10k) = table[3];
    if cfg!(debug_assertions) {
        println!(
            "timing gate = SKIP (unoptimized build; measured 1k={p95_1k:?} \
             10k={p95_10k:?}). Run with --release to enforce."
        );
    } else {
        assert!(
            p95_10k < FRAME_BUDGET,
            "cached 10k-row p95 must fit the 33ms live frame budget: {p95_10k:?}"
        );
        assert!(
            p95_10k < p95_1k * 4 + Duration::from_millis(5),
            "viewport rendering must stay bounded as history grows: \
             1k={p95_1k:?} 10k={p95_10k:?}"
        );
    }

    let ledger = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/OPTIMIZATIONS.md"),
    )
    .expect("the optimization ledger is readable");
    assert!(
        ledger.contains(SHIPPED_MARKER),
        "the optimization ledger must mark viewport caching as `{SHIPPED_MARKER}`"
    );

    let middle = replayed(10_000);
    let _ = p95_frame(&middle, 1);
    middle.scroll_back.set(middle.scroll_max.get() / 2);
    let middle_p95 = p95_frame(&middle, samples());
    println!("render p95 @ 10000 rows, middle = {middle_p95:?}");
    if !cfg!(debug_assertions) {
        assert!(
            middle_p95 < FRAME_BUDGET,
            "cached mid-scroll 10k-row p95 must fit the 33ms live frame budget: {middle_p95:?}"
        );
    }

    // The render/cache-fill loop is unchanged from origin/main. Measure three
    // genuinely fresh caches and take the best sample so a scheduler preempt
    // cannot masquerade as per-row work. A real regression slows all three.
    let _allocator_and_page_cache_prefill = cold_10k_frame();
    let cold_samples = [cold_10k_frame(), cold_10k_frame(), cold_10k_frame()];
    let first = minimum_duration(&cold_samples);
    println!("render cold-frame @ 10000 rows = {cold_samples:?}; min={first:?}");
    // MUTATION CHECK: selecting max instead of min is caught without relying
    // on host timing; this also pins the intended outlier policy.
    assert_eq!(
        minimum_duration(&[
            Duration::from_millis(300),
            Duration::from_millis(200),
            Duration::from_millis(400),
        ]),
        Duration::from_millis(200)
    );
    if !cfg!(debug_assertions) {
        assert!(
            first < Duration::from_millis(250),
            "even a cold 10k-row cache fill must stay under 250ms: {first:?}"
        );
    }
}
