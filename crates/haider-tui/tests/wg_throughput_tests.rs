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
    // One-second ticks across 40s of mock stream: enough closed 5s BUCKETS
    // (v0.0.937 bucket law) for a populated sparkline and μ/p95.
    let mut tok = 0u64;
    for i in 0..40u64 {
        tok += 100 + i * 6;
        model.throughput.observe(1_000 * (i + 1), tok, true);
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
fn wg3_off_stream_keeps_the_pill_visible() {
    let mut model = streaming_model();
    assert!(
        model.throughput_pill().is_some(),
        "precondition: streaming establishes a rate"
    );
    // The turn ends. The always-visible identity-line pill KEEPS the last
    // measured rate (owner: visible even when not streaming) — only the old
    // streaming-gated `throughput_readout` goes dark.
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Done,
    ))));
    assert!(
        model.throughput_readout().is_none(),
        "the streaming gate goes dark off-stream"
    );
    assert!(
        model.throughput_pill().is_some(),
        "the pill holds the last measured rate at rest"
    );
    let rows = draw(&model, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains("tps")),
        "the throughput pill is still on the identity line at rest"
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
fn wg3_streaming_shows_the_pill_on_the_identity_line() {
    // While streaming, the pill carries the live rate + sparkline on the
    // composer identity line (the band row is retired).
    let model = streaming_model();
    let rows = draw(&model, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains("tps")),
        "the identity-line pill carries the rate unit while streaming"
    );
    assert!(
        rows.iter()
            .any(|row| row.chars().any(|c| "▁▂▃▄▅▆▇█".contains(c))),
        "the sparkline renders while streaming"
    );
}

// ---- WG6: plain-mode parity + theme sweep ----

#[test]
fn wg6_styled_pill_and_plain_share_the_rate() {
    let model = streaming_model();
    let readout = model.throughput_pill().expect("streaming rate");
    // The two surfaces render throughput in different SHAPES (a compact pill
    // on the identity line vs the verbose plain row) but the SAME numbers —
    // the rate figure appears on both, so they cannot drift on the data.
    let rate = format!("{} tps", readout.tps);
    let rows = draw(&model, 100, 30);
    assert!(
        rows.iter().any(|row| row.contains(&rate)),
        "the styled identity-line pill carries the rate `{rate}`"
    );
    let plain = render_plain(&model.projection, 200_000, Some(&readout));
    assert!(
        plain.contains(&readout.plain_text()) && plain.contains(&rate),
        "plain mode prints the same rate:\n{plain}"
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
            .find(|row| row.contains("tps"))
            .unwrap_or_else(|| panic!("throughput pill missing in {key:?}"));
        // The rate and a sparkline glyph both survive the theme.
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
