//! tpsfix (v0.0.970) — the throughput widget's rendered shape and its wiring to
//! the session projection.
//!
//! Owner 2026-09-03: the widget must be a SMALL fixed-width strip (about a
//! quarter of its former ~40 columns), left-anchored, that does NOT grow with
//! the terminal — so the golden frames below are captured at 80, 118 and 160
//! columns and must be byte-identical to each other.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::provider::{Usage, UsageSource};
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use haider_tui::throughput::{PILL_WIDTH, SPARK_WIDTH, ThroughputPhase};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::launcher_model;

/// The three terminal widths every golden is captured at.
const WIDTHS: [u16; 3] = [80, 118, 160];

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

fn usage(output: u64, reasoning: u64) -> EventPayload {
    EventPayload::Usage(Usage {
        input: 12_000,
        output,
        reasoning,
        cached: 8_000,
        source: UsageSource::ProviderReported,
        account: None,
        accounts: Vec::new(),
        normalized: None,
        scope: None,
        cache_cost: None,
        request: None,
    })
}

fn rows(model: &AppModel, width: u16) -> Vec<String> {
    let backend = TestBackend::new(width, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            let _ = render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// The one row carrying the throughput widget, right-trimmed. Panics when the
/// widget is absent or drawn twice — both are regressions of their own.
fn widget_row(model: &AppModel, width: u16) -> String {
    let drawn = rows(model, width);
    let hits: Vec<&String> = drawn
        .iter()
        .filter(|row| row.contains(" tps") || row.contains('⋯'))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "exactly one throughput row at {width} columns, found {}:\n{}",
        hits.len(),
        drawn.join("\n")
    );
    hits[0].trim_end().to_owned()
}

/// A dark-theme session whose turn is OPEN at the given run state.
fn turn_open(state: RunState) -> AppModel {
    let mut model = launcher_model();
    model.theme = ThemeKey::Dark;
    model.handle(AppEvent::Envelope(Box::new(user_message("go"))));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(state))));
    model
}

/// Twenty seconds of steady generation at 200 chars/s (= 50 tok/s at the
/// default ratio) with the provider's exact cumulative total riding along, so
/// the readout is EXACT (no `~`) and four 5s buckets have closed.
fn seed_streaming(model: &mut AppModel) {
    let turn = model.projection.turn_epoch();
    model.throughput.observe(0, turn, 0, None);
    let mut chars = 0u64;
    for step in 1..=80u64 {
        chars += 50;
        model
            .throughput
            .observe(250 * step, turn, chars, Some(chars / 4));
    }
}

// ---- golden frames: thinking / streaming / finalized ----

#[test]
fn golden_thinking_shows_elapsed_and_never_a_zero_rate() {
    let mut model = turn_open(RunState::Thinking);
    let turn = model.projection.turn_epoch();
    for step in 0..=5u64 {
        model.throughput.observe(600 * step, turn, 0, None);
    }
    let readout = model.throughput_pill().expect("the turn is open");
    assert_eq!(readout.phase, ThroughputPhase::Warmup);
    assert_eq!(readout.tps, None);
    for width in WIDTHS {
        assert_eq!(
            widget_row(&model, width),
            "          ⋯ 3.0s",
            "thinking golden at {width} columns"
        );
    }
}

#[test]
fn golden_streaming_carries_the_measured_rate() {
    let mut model = turn_open(RunState::Streaming);
    seed_streaming(&mut model);
    let readout = model.throughput_pill().expect("a rate is established");
    assert_eq!(readout.phase, ThroughputPhase::Live);
    assert_eq!(readout.tps, Some(50));
    assert!(!readout.approx, "the exact usage frames drop the ~");
    for width in WIDTHS {
        assert_eq!(
            widget_row(&model, width),
            " ▁▁▁▁     50 tps",
            "streaming golden at {width} columns"
        );
    }
}

#[test]
fn golden_streaming_estimate_wears_the_tilde() {
    // The same stream with NO provider usage: the byte-derived estimate is
    // marked, and the `~` costs a digit column rather than a new cell.
    let mut model = turn_open(RunState::Streaming);
    let turn = model.projection.turn_epoch();
    model.throughput.observe(0, turn, 0, None);
    let mut chars = 0u64;
    for step in 1..=80u64 {
        chars += 50;
        model.throughput.observe(250 * step, turn, chars, None);
    }
    for width in WIDTHS {
        assert_eq!(
            widget_row(&model, width),
            " ▁▁▁▁    ~50 tps",
            "approximate golden at {width} columns"
        );
    }
}

#[test]
fn golden_finalized_shows_the_turn_mean() {
    let mut model = turn_open(RunState::Streaming);
    seed_streaming(&mut model);
    let turn = model.projection.turn_epoch();
    // The provider's final figure: 700 output tokens over the 20s generation
    // span = 35 tps, which is the turn's own mean and NOT the last window's
    // 50 tps — the `166 tps` in the owner's screenshot was that last window.
    model.throughput.settle(21_000, turn, Some(700));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    let readout = model.throughput_pill().expect("the pill persists at rest");
    assert_eq!(readout.phase, ThroughputPhase::Settled);
    assert_eq!(readout.tps, Some(35));
    for width in WIDTHS {
        assert_eq!(
            widget_row(&model, width),
            " ▁▁▁▁     35 tps",
            "finalized golden at {width} columns"
        );
    }
}

// ---- the fixed-width law ----

#[test]
fn the_widget_is_a_fixed_cell_budget_that_never_scales_with_the_terminal() {
    let mut model = turn_open(RunState::Streaming);
    seed_streaming(&mut model);
    let mut seen = Vec::new();
    for width in [60_u16, 80, 100, 118, 140, 160, 200] {
        let row = widget_row(&model, width);
        // One leading margin cell, then exactly PILL_WIDTH cells of widget.
        assert_eq!(
            row.chars().count(),
            PILL_WIDTH + 1,
            "the widget is fixed at {width} columns: {row:?}"
        );
        seen.push(row);
    }
    assert!(
        seen.windows(2).all(|pair| pair[0] == pair[1]),
        "the widget is byte-identical across terminal widths: {seen:?}"
    );
}

#[test]
fn the_widget_is_about_a_quarter_of_the_retired_row() {
    // The pre-tpsfix row was a 24-column sparkline plus a 5-cell rate field,
    // ` tps` and a `· μN` tail — ~40 cells. The owner asked for roughly a
    // quarter of that, in a FIXED 12–16 cell budget; this pins it so the row
    // cannot creep back.
    /// Sparkline (24) + rate field (5) + ` tps` (4) + ` · μNNN` (7).
    const RETIRED_WIDTH: usize = 40;
    assert_eq!(SPARK_WIDTH, 6);
    assert_eq!(PILL_WIDTH, 15);
    assert!(
        (12..=16).contains(&PILL_WIDTH),
        "the owner's fixed 12–16 cell budget: {PILL_WIDTH}"
    );
    assert_eq!(
        RETIRED_WIDTH - PILL_WIDTH,
        25,
        "25 cells of terminal handed back per row"
    );
}

#[test]
fn the_widget_carries_no_mean_tail() {
    let mut model = turn_open(RunState::Streaming);
    seed_streaming(&mut model);
    let readout = model.throughput_pill().unwrap();
    assert!(
        readout.mean.is_some(),
        "the closed-bucket mean still exists for the verbose plain row"
    );
    for width in WIDTHS {
        let row = widget_row(&model, width);
        assert!(!row.contains('μ'), "no μ tail on the widget: {row:?}");
        assert!(!row.contains("p95"), "no p95 tail on the widget: {row:?}");
    }
}

#[test]
fn a_four_figure_rate_still_fits_the_field() {
    let mut model = turn_open(RunState::Streaming);
    let turn = model.projection.turn_epoch();
    model.throughput.observe(0, turn, 0, None);
    let mut chars = 0u64;
    for step in 1..=80u64 {
        chars += 1_000;
        model
            .throughput
            .observe(250 * step, turn, chars, Some(chars / 4));
    }
    let readout = model.throughput_pill().unwrap();
    assert_eq!(readout.tps, Some(1_000));
    for width in WIDTHS {
        let row = widget_row(&model, width);
        assert_eq!(row.chars().count(), PILL_WIDTH + 1, "{row:?}");
        assert!(row.contains("1000 tps"), "{row:?}");
    }
}

#[test]
fn every_theme_draws_the_same_glyphs() {
    for key in ThemeKey::ALL {
        let mut model = turn_open(RunState::Streaming);
        model.theme = key;
        seed_streaming(&mut model);
        assert_eq!(
            widget_row(&model, 118),
            " ▁▁▁▁     50 tps",
            "the widget's GLYPHS are theme-invariant ({key:?}); only ink changes"
        );
    }
}

// ---- wiring: the turn epoch is what makes the numbers honest ----

#[test]
fn a_usage_frame_from_the_previous_turn_is_not_this_turn_s_total() {
    // The root cause of the owner's `0 tps` → five-figure jump: `Usage::output`
    // is cumulative within a RUN, and the projection keeps the last frame after
    // the run ends. Reading it during the next turn made the tracker start at
    // the previous turn's total and then leap by a whole usage frame.
    let mut model = turn_open(RunState::Streaming);
    assert_eq!(model.projection.turn_epoch(), 1);
    model.handle(AppEvent::Envelope(Box::new(usage(2_100, 900))));
    assert_eq!(
        model.projection.turn_output_tokens_exact(),
        Some(2_100),
        "the frame belongs to the turn that produced it"
    );
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    assert!(!model.projection.turn_live());
    assert_eq!(
        model.projection.turn_output_tokens_exact(),
        Some(2_100),
        "a frame committed at the terminal still settles its own turn"
    );

    // Turn two opens. The retained frame is now STALE and must read as absent.
    model.handle(AppEvent::Envelope(Box::new(user_message("again"))));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Thinking,
    ))));
    assert_eq!(model.projection.turn_epoch(), 2);
    assert_eq!(
        model.projection.turn_output_tokens_exact(),
        None,
        "turn two has reported no usage of its own yet"
    );
    assert!(model.projection.turn_live());
}

#[test]
fn note_throughput_observes_a_live_turn_and_settles_a_finished_one() {
    let mut model = turn_open(RunState::Streaming);
    let turn = model.projection.turn_epoch();
    // A second of silent thinking on the frame clock: warm-up, no rate. The
    // last zero-output observation (t = 1000) becomes the generation clock's
    // zero, so time-to-first-token is excluded from everything below.
    for step in 0..=2u64 {
        model.clock_ms = 500 * step;
        model.note_throughput();
    }
    let warmup = model.throughput_pill().expect("the turn is open");
    assert_eq!(warmup.phase, ThroughputPhase::Warmup);
    assert_eq!(warmup.tps, None, "never a fabricated 0 tps while thinking");
    // Then the tracker is driven directly (the projection's character counter
    // is fed by item deltas, which this law does not exercise) and the run goes
    // terminal — `note_throughput` must SETTLE rather than keep observing.
    let mut chars = 0u64;
    for step in 1..=80u64 {
        chars += 50;
        model
            .throughput
            .observe(1_000 + 250 * step, turn, chars, None);
    }
    assert_eq!(
        model.throughput_pill().unwrap().phase,
        ThroughputPhase::Live
    );
    model.handle(AppEvent::Envelope(Box::new(usage(700, 0))));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    model.clock_ms = 23_000;
    model.note_throughput();
    let settled = model.throughput_pill().expect("the pill persists at rest");
    assert_eq!(settled.phase, ThroughputPhase::Settled);
    assert_eq!(
        settled.tps,
        Some(35),
        "700 provider-reported tokens over the 20s generation span"
    );
    assert!(
        !settled.approx,
        "the provider's own figure is not an estimate"
    );
}

#[test]
fn a_parked_turn_is_not_settled_early() {
    // A permission menu, a tool, or provider backoff parks a turn WITHOUT
    // ending it. Settling there would publish a final figure mid-turn.
    for state in [
        RunState::RunningTool,
        RunState::Compacting,
        RunState::Waiting {
            reason: haider_protocol::state::WaitReason::ProviderBackoff,
        },
        RunState::Queued,
    ] {
        let model = turn_open(state.clone());
        assert!(
            model.projection.turn_live(),
            "{state:?} has not ended the turn"
        );
    }
    for state in [RunState::Done, RunState::Cancelled, RunState::Errored] {
        let model = turn_open(state.clone());
        assert!(!model.projection.turn_live(), "{state:?} ended the turn");
    }
}
