//! W-G render laws for the live throughput row: the idle no-op (WG3) and the
//! plain-mode + theme parity (WG6). The pure tps/μ/p95/sparkline math (WG1,
//! WG2), the streaming rise + reset (WG4) and the fallback honesty (WG5) live
//! as unit laws in `src/throughput.rs`; these cover the render surface those
//! laws feed.
#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel};
use haider_tui::plain::render_plain;
use haider_tui::render::render;
use haider_tui::theme::ThemeKey;
use haider_tui::throughput::ThroughputReadout;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::launcher_model;

fn user_message(text: &str) -> EventPayload {
    EventPayload::UserMessage {
        text: text.to_owned(),
        attachments: Vec::new(),
        mode: haider_protocol::DeliveryMode::Steer,
    }
}

fn draw(model: &AppModel, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
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

/// Feed a scripted, rising token stream (exact usage) so the tracker holds a
/// full readout — enough samples for μ/p95, a populated sparkline.
fn seed_stream(model: &mut AppModel) {
    let mut tok = 0u64;
    for i in 0..10u64 {
        tok += 100 + i * 6;
        model.throughput.observe(250 * (i + 1), tok, true);
    }
}

/// A session on the Session screen, mid-stream, with a populated tracker.
fn streaming_model() -> AppModel {
    let mut model = launcher_model();
    model.handle(AppEvent::Envelope(Box::new(user_message("go"))));
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Streaming,
    ))));
    seed_stream(&mut model);
    model
}

// ---- WG3: idle no-op (the row is absent and idle frames are byte-identical) ----

#[test]
fn wg3_off_stream_hides_the_row_even_with_a_populated_tracker() {
    let mut model = streaming_model();
    assert!(
        model.throughput_readout().is_some(),
        "precondition: streaming shows a readout"
    );
    // The turn ends. The tracker still holds its samples (the live runtime
    // resets it on the next idle tick), but the render GATE must hide the row
    // the instant the run leaves a streaming state.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    assert!(
        model.throughput_readout().is_none(),
        "off-stream: the gate hides the row regardless of tracker contents"
    );
    let rows = draw(&model, 100, 30);
    assert!(
        !rows.iter().any(|row| row.contains("Throughput")),
        "no throughput row when the turn is not streaming"
    );
}

#[test]
fn wg3_idle_frames_are_byte_identical_across_ticks() {
    let mut model = streaming_model();
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    // Two ticks of the phase clock: with the throughput row absent (and the
    // rest of an idle Done session at rest) the frames must match exactly —
    // the row costs nothing when no turn streams.
    model.anim_phase = 0;
    let at_zero = draw(&model, 100, 30);
    model.anim_phase = 200;
    let at_tick = draw(&model, 100, 30);
    assert_eq!(at_zero, at_tick, "idle frames are phase-invariant (WG3)");
}

#[test]
fn wg3_streaming_shows_the_row() {
    // The both-directions half of the gate: while streaming, the row IS drawn.
    let model = streaming_model();
    let rows = draw(&model, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains("Throughput")),
        "the throughput row is drawn while the turn streams"
    );
    assert!(
        rows.iter().any(|row| row.contains("tps")),
        "the row carries the rate unit"
    );
}

// ---- WG6: plain-mode parity + theme sweep ----

#[test]
fn wg6_styled_row_matches_the_plain_readout_text() {
    let model = streaming_model();
    let readout = model.throughput_readout().expect("streaming readout");
    // The styled render row, stripped of padding, equals the plain-mode line
    // built from the SAME readout — the two surfaces cannot drift.
    let rows = draw(&model, 100, 30);
    let styled = rows
        .iter()
        .find(|row| row.contains("Throughput"))
        .expect("styled throughput row present");
    assert_eq!(styled.trim(), readout.plain_text());
    // And the plain renderer emits exactly that line when a readout is present.
    let plain = render_plain(&model.projection, 200_000, Some(&readout));
    assert!(
        plain.contains(&readout.plain_text()),
        "plain mode prints the equivalent line:\n{plain}"
    );
}

#[test]
fn wg6_plain_omits_the_row_when_idle() {
    // No readout → the plain output is unchanged (no fabricated row).
    let model = launcher_model();
    let plain = render_plain(&model.projection, 200_000, None);
    assert!(!plain.contains("Throughput"));
}

#[test]
fn wg6_row_renders_legibly_in_every_theme() {
    for key in ThemeKey::ALL {
        let mut model = streaming_model();
        model.theme = key;
        let rows = draw(&model, 100, 30);
        let row = rows
            .iter()
            .find(|row| row.contains("Throughput"))
            .unwrap_or_else(|| panic!("throughput row missing in {key:?}"));
        // The label, a sparkline glyph and the rate all survive the theme.
        assert!(row.contains("Throughput"), "{key:?}");
        assert!(row.contains("tps"), "{key:?}");
        assert!(
            row.chars().any(|c| "▁▂▃▄▅▆▇█".contains(c)),
            "the sparkline renders in {key:?}: {row}"
        );
    }
}

#[test]
fn wg6_approx_readout_wears_the_tilde_in_plain_and_styled() {
    // A fallback (approximate) readout: the ~ must appear on both surfaces so
    // an estimated rate never reads as a measured one.
    let approx = ThroughputReadout {
        spark: "▁▂▃".to_owned(),
        tps: 126,
        approx: true,
        mean: Some(119),
        p95: Some(154),
    };
    assert_eq!(
        approx.plain_text(),
        "Throughput ▁▂▃ ~126 tps · μ 119 · p95 154"
    );
    let plain = render_plain(&AppModel::new().projection, 0, Some(&approx));
    assert!(plain.contains("~126 tps"));
}
