//! tuivirt shape gate — the replacement for optimization-ledger row 17's
//! benchmark (`w3c3_render_bench_tests`), stated as the re-architecture's
//! TARGET shape rather than today's calibrated bounds:
//!
//! * the FIRST frame of a freshly attached session is ≤ 33 ms at 10k, 50k
//!   and 200k rows (no O(N) cold cache fill);
//! * the cached p95 stays ≤ 33 ms at every size, following and mid-scroll;
//! * both are FLAT: first-frame medians use a 1.65× ratio, cached p95 a
//!   1.20× ratio, each with 1 ms jitter slack (see the cost model below).
//!   A failed timing comparison gets one logged, complete remeasurement.
//!
//! ONLY AN OPTIMIZED BUILD MEASURES THE THRESHOLDS (the row-17 bench's own
//! discipline): a debug build prints a loud SKIP and asserts no timing bounds.
//!
//! ```text
//! cargo test --release -p haider-tui --test tuivirt_shape_bench_tests --locked -- --nocapture
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
/// For this all-agent-row fixture, seed_estimates and viewport formatting
/// are O(1) in N (fixed 118×36 viewport, bounded cache); entry_at_row is
/// O(log N), with at most ceil(log2 N) = 14 vs 18 binary-search steps.
/// Even if lookup dominated, 18/14 × 1.25 = 1.6072 would give 25% headroom;
/// 1.65 gives 28.3%. Replay and its O(N) allocation volume are outside the timer.
/// A discarded full-size construction warms allocator high-water/page state,
/// but every measured model/terminal still has an empty render cache. Median
/// sampling and fixed jitter slack cover residual scheduling/allocation noise,
/// not an N-proportional allowance for scanning or formatting history.
const FIRST_FLAT_RATIO: f64 = 1.65;
const CACHED_FLAT_RATIO: f64 = 1.20;
const FLAT_SLACK: Duration = Duration::from_millis(1);
const FIRST_SAMPLES: usize = 5;
const SIZES: [usize; 3] = [10_000, 50_000, 200_000];

#[cfg(debug_assertions)]
thread_local! {
    // Count the real constructions across the entire behavioral test, so an
    // accidental warm-up or retry fails BEFORE repeating expensive replay.
    static COLD_CONSTRUCTIONS: std::cell::Cell<[usize; 3]> = const {
        std::cell::Cell::new([0; 3])
    };
}

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

#[cfg(debug_assertions)]
fn debug_replayed(rows: usize) -> AppModel {
    // Completed-only replay scans the whole prior transcript for an open
    // item at each append. Keep debug fixture setup linear: reduce each row
    // in isolation, then hydrate the same display entries into ONE model.
    // Release still exercises the original replay, outside its frame timers.
    let mut model = tuivirt_common::session_model();
    let entries = (0..rows)
        .map(|n| {
            tuivirt_common::push_agent(
                &mut model,
                &format!("bench-{n}"),
                &tuivirt_common::agent_row(n),
            );
            let row = std::mem::take(&mut model.projection);
            let [entry] = row.entries() else {
                panic!("one completed agent event must produce exactly one display entry");
            };
            entry.clone()
        })
        .collect();
    model.projection =
        haider_tui::projection::SessionProjection::hydrate(entries, None, None, None, false);
    model
}

#[cfg(debug_assertions)]
fn assert_debug_fixture_matches_replay() {
    use tuivirt_common::{assert_same_frame, draw};

    for rows in [0, 1, 64] {
        let expected = replayed(rows);
        let actual = debug_replayed(rows);
        assert_eq!(actual.projection.entries(), expected.projection.entries());
        assert_same_frame(
            "fixture tail",
            &draw(&actual, 118, 36),
            &draw(&expected, 118, 36),
        );
        assert_eq!(actual.scroll_max.get(), expected.scroll_max.get());
        for back in [expected.scroll_max.get() / 2, expected.scroll_max.get()] {
            actual.scroll_back.set(back);
            expected.scroll_back.set(back);
            assert_same_frame(
                "fixture scroll",
                &draw(&actual, 118, 36),
                &draw(&expected, 118, 36),
            );
        }
    }
}

fn cold_frame(rows: usize) -> (AppModel, Terminal<TestBackend>, Duration) {
    #[cfg(debug_assertions)]
    COLD_CONSTRUCTIONS.with(|counts| {
        let mut actual = counts.get();
        let index = SIZES
            .iter()
            .position(|&size| size == rows)
            .expect("bench size");
        assert_eq!(
            actual[index], 0,
            "debug shape probe must construct at most once per size ({rows} rows)"
        );
        actual[index] += 1;
        counts.set(actual);
    });
    // Fresh construction, never a cloned or previously rendered model.
    // Replay and terminal construction stay outside every interval.
    #[cfg(debug_assertions)]
    let model = debug_replayed(rows);
    #[cfg(not(debug_assertions))]
    let model = replayed(rows);
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    let first = one_frame(&model, &mut terminal);
    let first_frame_text = frame_text(&terminal);
    assert!(
        first_frame_text.contains(&format!("row {}", rows - 1)),
        "the first frame must open on the real visible tail at {rows} rows"
    );
    (model, terminal, first)
}

#[cfg(not(debug_assertions))]
fn measure(rows: usize) -> Shape {
    // Discard one warm-up AT THIS SIZE, then measure five independent cold
    // render caches in this process. Drop each before constructing the next;
    // retain only the last model/terminal for the unchanged cached p95 probes.
    drop(cold_frame(rows));
    let mut first_samples: Vec<Duration> = (1..FIRST_SAMPLES).map(|_| cold_frame(rows).2).collect();
    let (model, terminal, first) = cold_frame(rows);
    first_samples.push(first);
    probe_cached(rows, model, terminal, percentile(first_samples, 50))
}

fn probe_cached(
    rows: usize,
    model: AppModel,
    mut terminal: Terminal<TestBackend>,
    first: Duration,
) -> Shape {
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

fn flat(what: &str, small: Duration, large: Duration, ratio: f64) -> Result<(), String> {
    let ceiling = small.mul_f64(ratio) + FLAT_SLACK;
    if large > ceiling {
        return Err(format!(
            "{what} must stay flat from 10k to 200k rows: 10k={small:?} 200k={large:?} \
             ratio={:.3} (ceiling {ceiling:?}, rule {ratio:.2}× + {FLAT_SLACK:?})",
            large.as_secs_f64() / small.as_secs_f64()
        ));
    }
    Ok(())
}

fn check_shapes(shapes: &[Shape; 3]) -> Result<(), String> {
    for shape in shapes {
        for (what, timing) in [
            ("first-frame median", shape.first),
            ("cached following p95", shape.p95_follow),
            ("cached mid-scroll p95", shape.p95_middle),
        ] {
            if timing > FRAME_BUDGET {
                return Err(format!(
                    "{what} @ {} rows must fit the 33 ms budget: {timing:?}",
                    shape.rows
                ));
            }
        }
    }
    let (small, large) = (&shapes[0], &shapes[2]);
    flat(
        "first-frame median",
        small.first,
        large.first,
        FIRST_FLAT_RATIO,
    )?;
    flat(
        "cached following p95",
        small.p95_follow,
        large.p95_follow,
        CACHED_FLAT_RATIO,
    )?;
    flat(
        "cached mid-scroll p95",
        small.p95_middle,
        large.p95_middle,
        CACHED_FLAT_RATIO,
    )
}

fn compare_with_retry(
    label: &str,
    mut measure_all: impl FnMut() -> [Shape; 3],
) -> Result<(), String> {
    for attempt in 1..=2 {
        // Never mix the best sizes from separate attempts: repeat ALL sizes,
        // cold constructions, cached probes and absolute/relative checks.
        let shapes = measure_all();
        for shape in &shapes {
            println!(
                "tuivirt shape ({label}) attempt {attempt}/2 @ {} rows: first median(n={FIRST_SAMPLES})={:?} \
                 p95(follow)={:?} p95(middle)={:?}",
                shape.rows, shape.first, shape.p95_follow, shape.p95_middle
            );
        }
        println!(
            "tuivirt ({label}) first-frame median ratio 200k/10k={:.3}; ceiling={:?} ({FIRST_FLAT_RATIO:.2}× + {FLAT_SLACK:?})",
            shapes[2].first.as_secs_f64() / shapes[0].first.as_secs_f64(),
            shapes[0].first.mul_f64(FIRST_FLAT_RATIO) + FLAT_SLACK
        );
        match check_shapes(&shapes) {
            Ok(()) => return Ok(()),
            Err(reason) if attempt == 1 => eprintln!(
                "tuivirt shape ({label}) attempt 1/2 failed: {reason}; retrying the whole comparison once \
                 to distinguish shared-runner noise from persistent growth"
            ),
            Err(reason) => return Err(format!("tuivirt shape failed after 2 attempts: {reason}")),
        }
    }
    unreachable!("both attempts return or retry")
}

#[test]
fn first_frame_and_cached_p95_are_flat_from_10k_to_200k_rows() {
    #[cfg(debug_assertions)]
    {
        println!("tuivirt shape gate = SKIP (unoptimized build). Run with --release to enforce.");
        // Keep the visible-tail and full-scroll behavioral assertions active.
        for rows in SIZES {
            let (model, terminal, first) = cold_frame(rows);
            let _ = probe_cached(rows, model, terminal, first);
        }
        COLD_CONSTRUCTIONS.with(|counts| {
            assert_eq!(counts.get(), [1; 3], "one debug construction at every size");
            println!(
                "tuivirt debug constructions @ {SIZES:?} rows: {:?}",
                counts.get()
            );
        });
    }
    #[cfg(not(debug_assertions))]
    compare_with_retry("measured", || SIZES.map(measure))
        .unwrap_or_else(|reason| panic!("{reason}"));
}

/// MUTATION CHECK for the gate's own arithmetic, always on: the flatness
/// ceilings are pinned independently, and the percentile picks the right sample.
#[test]
fn shape_gate_arithmetic_is_pinned() {
    #[cfg(debug_assertions)]
    assert_debug_fixture_matches_replay();
    flat(
        "exactly flat",
        Duration::from_millis(10),
        Duration::from_millis(10),
        CACHED_FLAT_RATIO,
    )
    .unwrap();
    flat(
        "within 20 %",
        Duration::from_millis(10),
        Duration::from_millis(12),
        CACHED_FLAT_RATIO,
    )
    .unwrap();
    flat(
        "slack covers jitter",
        Duration::from_micros(500),
        Duration::from_micros(1500),
        CACHED_FLAT_RATIO,
    )
    .unwrap();
    let result = std::panic::catch_unwind(|| {
        flat(
            "too steep",
            Duration::from_millis(10),
            Duration::from_millis(14),
            CACHED_FLAT_RATIO,
        )
        .unwrap();
    });
    assert!(result.is_err(), "a 40 % rise must fail the flatness gate");
    flat(
        "first boundary",
        Duration::from_millis(10),
        Duration::from_micros(17_500),
        FIRST_FLAT_RATIO,
    )
    .unwrap();
    let result = std::panic::catch_unwind(|| {
        flat(
            "first above boundary",
            Duration::from_millis(10),
            Duration::from_nanos(17_500_001),
            FIRST_FLAT_RATIO,
        )
        .unwrap();
    });
    assert!(
        result.is_err(),
        "first-frame ratio and slack must not widen"
    );
    assert_eq!(FIRST_SAMPLES, 5);
    let cold = [9, 1, 50, 3, 2].map(Duration::from_millis).to_vec();
    assert_eq!(percentile(cold, 50), Duration::from_millis(3));
    let timings = (1..=100u64).map(Duration::from_millis).collect::<Vec<_>>();
    assert_eq!(percentile(timings.clone(), 95), Duration::from_millis(96));
    assert_eq!(percentile(timings, 50), Duration::from_millis(51));
}

#[test]
fn retry_requires_one_complete_passing_comparison() {
    let shapes = |first: [u64; 3]| {
        std::array::from_fn(|i| Shape {
            rows: SIZES[i],
            first: Duration::from_millis(first[i]),
            p95_follow: Duration::from_millis(1),
            p95_middle: Duration::from_millis(1),
        })
    };
    for (attempts, passes) in [
        (vec![[1, 1, 1]], true),
        (vec![[1, 5, 20], [1, 1, 1]], true),
        (vec![[1, 5, 20], [1, 5, 20]], false),
        // Mixing small=10 from the first attempt and large=3 from the
        // second would pass, but neither whole comparison is valid.
        (vec![[10, 34, 10], [1, 1, 3]], false),
    ] {
        let expected_calls = attempts.len();
        let mut attempts = attempts.into_iter();
        let mut calls = 0;
        let result = compare_with_retry("synthetic retry pin", || {
            calls += 1;
            shapes(attempts.next().expect("at most one retry"))
        });
        assert_eq!(result.is_ok(), passes);
        assert_eq!(calls, expected_calls);
    }
    // The absolute ceilings and the unchanged cached flatness gates remain
    // independently enforceable, including the intermediate 50k size.
    for index in 0..3 {
        for metric in 0..3 {
            let mut sample = shapes([1, 1, 1]);
            let timing = match metric {
                0 => &mut sample[index].first,
                1 => &mut sample[index].p95_follow,
                _ => &mut sample[index].p95_middle,
            };
            *timing = FRAME_BUDGET + Duration::from_nanos(1);
            assert!(check_shapes(&sample).is_err());
        }
    }
    let mut sample = shapes([1, 1, 1]);
    sample[2].p95_follow = Duration::from_millis(3);
    assert!(check_shapes(&sample).is_err());
    sample[2].p95_follow = Duration::from_millis(1);
    sample[2].p95_middle = Duration::from_millis(3);
    assert!(check_shapes(&sample).is_err());
}
