//! Lane voicefix — the LISTENING frame-time gate (owner requirement 3:
//! "bar/blink updates must not slow the TUI").
//!
//! The wave and the blink are the only chrome that redraws on a timer
//! while nothing else in the session moves, so the cost that matters is
//! the cost of ONE frame drawn while `/talk` is listening over a real
//! transcript. Two shapes are measured at the bench's 118x36:
//!
//! * `speaking` — one envelope applied before each frame (the live mic
//!   cadence), so every frame re-reads a mutated wave ring;
//! * `silent`  — no envelope at all, only the wave clock advancing (the
//!   synthesized listening sweep + the blink).
//!
//! Both must stay inside the 33 ms frame budget the 30 fps coalescer
//! gives them, and the transcript underneath must NOT be re-laid-out per
//! level tick (tuivirt's viewport cache holds), which is what keeps the
//! numbers flat against the same model drawn while idle.
//!
//! ONLY AN OPTIMIZED BUILD MEASURES THE THRESHOLDS (the row-17 bench's
//! discipline): a debug build prints a loud SKIP and asserts nothing.
//!
//! ```text
//! cargo test --release -p haider-tui --test voicefix_frame_bench_tests -- --nocapture
//! ```
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_tui::app::AppModel;
use haider_tui::render::render;
use haider_tui::talk::{TalkEvent, TalkPhase};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::time::{Duration, Instant};

mod tuivirt_common;
use tuivirt_common::replayed;

/// The 30 fps coalescer's per-frame budget.
const FRAME_BUDGET: Duration = Duration::from_millis(33);
/// A transcript big enough that a re-layout regression would be obvious.
const ROWS: usize = 10_000;

fn samples() -> usize {
    if cfg!(debug_assertions) { 5 } else { 240 }
}

/// A live session over `rows` committed rows, with `/talk` LISTENING.
fn listening(rows: usize) -> AppModel {
    let mut model = replayed(rows);
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    assert_eq!(
        model.talk.phase,
        TalkPhase::Listening,
        "the bench must measure a genuinely listening frame"
    );
    model
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

fn percentile(mut timings: Vec<Duration>, percentile: usize) -> Duration {
    timings.sort_unstable();
    timings[(timings.len() * percentile / 100).min(timings.len() - 1)]
}

/// A speech-shaped envelope level for sample `n` — a slow swell with a
/// couple of louder syllables, so the ring is genuinely mutated (and the
/// hot/quiet split genuinely flips) between frames.
fn speech_level(n: usize) -> f32 {
    let t = n as f32 * 0.13;
    let swell = 0.5f32.mul_add(t.sin(), 0.5);
    let syllable = if n % 7 < 3 { 1.0 } else { 0.35 };
    (0.06 + 0.12 * swell * syllable).clamp(0.0, 1.0)
}

struct Shape {
    p50: Duration,
    p95: Duration,
    worst: Duration,
}

fn measure(label: &str, speaking: bool) -> Shape {
    // Model and terminal construction stay outside every timed interval.
    let mut model = listening(ROWS);
    let generation = model.talk.generation;
    let mut terminal = Terminal::new(TestBackend::new(118, 36)).expect("test terminal");
    // Warm the viewport cache exactly as a real first frame would.
    let _ = one_frame(&model, &mut terminal);
    let mut timings = Vec::with_capacity(samples());
    for n in 0..samples() {
        if speaking {
            model.handle_talk(TalkEvent::Envelope {
                generation,
                level: speech_level(n),
            });
        }
        // The wave clock advances every frame either way — that is what
        // the listening tick does at 30 fps.
        model.clock_ms += 33;
        timings.push(one_frame(&model, &mut terminal));
    }
    let worst = timings.iter().copied().max().unwrap_or_default();
    let shape = Shape {
        p50: percentile(timings.clone(), 50),
        p95: percentile(timings, 95),
        worst,
    };
    println!(
        "voicefix listening frame [{label}] rows={ROWS} samples={} p50={:?} p95={:?} worst={:?}",
        samples(),
        shape.p50,
        shape.p95,
        shape.worst
    );
    shape
}

/// The gate: a listening frame — speaking or silent — stays inside the
/// 30 fps budget over a 10k-row transcript.
///
/// MUTATION CHECK: make the wave rebuild the transcript layout every
/// level tick (drop tuivirt's viewport cache on a talk redraw). Expected
/// failure: `speaking` p95 leaves the 33 ms budget at 10k rows.
#[test]
fn a_listening_frame_stays_inside_the_thirty_fps_budget() {
    if cfg!(debug_assertions) {
        println!(
            "SKIP voicefix_frame_bench: debug build measures nothing \
             (run with --release to enforce the frame budget)"
        );
        let _ = measure("speaking(debug)", true);
        let _ = measure("silent(debug)", false);
        return;
    }
    let speaking = measure("speaking", true);
    let silent = measure("silent", false);
    for (what, shape) in [("speaking", &speaking), ("silent", &silent)] {
        assert!(
            shape.p95 <= FRAME_BUDGET,
            "a {what} listening frame must stay inside the 30 fps budget: \
             p95={:?} budget={FRAME_BUDGET:?}",
            shape.p95
        );
    }
}
