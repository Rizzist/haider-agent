//! T2 — the right-to-left ASCII wave's deterministic laws: ring geometry
//! (newest at the RIGHT edge, history flowing left), the sqrt glyph
//! mapping, the asymmetric attack/decay smoothing, noise-floor
//! calibration, the hot/quiet ink split, the plain-glyph fallback, and
//! the composer-band render integration.
#![allow(clippy::expect_used)]
#![allow(clippy::float_cmp)]

use haider_tui::app::{AppModel, RuntimeMode, Screen};
use haider_tui::render::render;
use haider_tui::talk::{
    TalkEvent, TalkPhase, WAVE_ATTACK, WAVE_DECAY, WAVE_GLYPHS_BLOCKS, WAVE_GLYPHS_PLAIN,
    WAVE_HOT_LEVEL, WAVE_WIDTH, WaveCell, WaveRing, wave_glyph, wave_glyph_index,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::launcher_model;

fn draw(model: &AppModel, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn sid() -> haider_protocol::ids::SessionId {
    haider_protocol::ids::SessionId::new("s-wave")
}

/// A live session with a talk session LISTENING (generation 1).
fn listening_model() -> AppModel {
    let mut model = launcher_model();
    model.mode = RuntimeMode::Live;
    model.sessions.clear();
    model.upsert_live_session(&sid());
    model.open_session(&sid());
    assert_eq!(model.screen, Screen::Session);
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    assert_eq!(model.talk.phase, TalkPhase::Listening);
    model
}

/// MUTATION CHECK: swap `push_back`/`pop_front` for front insertion (the
/// left-to-right shape). Expected failure: the newest sample lands at
/// index 0, not the RIGHT edge, and the shifted history assert breaks.
#[test]
fn ring_is_fixed_width_with_newest_at_the_right_edge() {
    let mut ring = WaveRing::new();
    assert_eq!(ring.levels().len(), WAVE_WIDTH);
    assert!(ring.levels().iter().all(|&level| level == 0.0));
    // Calibrate the floor at zero so pushes translate directly.
    ring.push(0.0);
    ring.push(1.0);
    let after_first = ring.levels();
    assert_eq!(after_first.len(), WAVE_WIDTH, "the ring never grows");
    let newest = after_first[WAVE_WIDTH - 1];
    assert!(newest > 0.0, "the newest sample enters at the right edge");
    ring.push(1.0);
    let after_second = ring.levels();
    // History flowed LEFT: the previous newest now sits one cell left.
    assert_eq!(after_second[WAVE_WIDTH - 2], newest);
    assert!(after_second[WAVE_WIDTH - 1] > newest);
}

/// MUTATION CHECK: swap the attack constant for the decay (symmetric
/// smoothing). Expected failure: one loud sample from silence lands at
/// 0.13, not 0.5.
#[test]
fn attack_rises_at_half_per_sample() {
    let mut ring = WaveRing::new();
    ring.push(0.0); // floor := 0
    ring.push(1.0);
    assert_eq!(ring.current(), WAVE_ATTACK * 1.0);
    ring.push(1.0);
    assert_eq!(ring.current(), 0.5 + (1.0 - 0.5) * WAVE_ATTACK);
}

/// MUTATION CHECK: drop the asymmetry (decay at 0.5). Expected failure:
/// the fall from 0.75 lands at 0.375 instead of the slow 0.6525 tail.
#[test]
fn decay_falls_at_thirteen_percent_per_sample() {
    let mut ring = WaveRing::new();
    ring.push(0.0);
    ring.push(1.0);
    ring.push(1.0);
    let peak = ring.current();
    assert_eq!(peak, 0.75);
    ring.push(0.0);
    let expected = peak + (0.0 - peak) * WAVE_DECAY;
    assert!((ring.current() - expected).abs() < 1e-6);
}

/// MUTATION CHECK: delete the floor subtraction (calibrated = raw).
/// Expected failure: constant ambient hum renders as a visible wave
/// instead of a flat baseline, and the speech sample maps to 0.6 raw
/// instead of 0.5 calibrated headroom.
#[test]
fn noise_floor_calibration_flattens_ambient_and_rescales_speech() {
    let mut ring = WaveRing::new();
    for _ in 0..10 {
        ring.push(0.2); // steady ambient
    }
    assert_eq!(ring.current(), 0.0, "ambient at the floor reads flat");
    assert!((ring.floor() - 0.2).abs() < 1e-6);
    ring.push(0.6);
    // (0.6 - 0.2) / (1 - 0.2) = 0.5 calibrated; one attack step = 0.25.
    assert!((ring.current() - 0.25).abs() < 1e-5);
}

/// MUTATION CHECK: replace the sqrt with a linear mapping. Expected
/// failure: 0.25 maps to index 2 instead of the perceptual 4.
#[test]
fn glyph_mapping_is_sqrt_and_total_over_the_unit_interval() {
    assert_eq!(wave_glyph_index(0.0), 0);
    assert_eq!(wave_glyph_index(1.0), 7);
    assert_eq!(wave_glyph_index(0.25), 4, "sqrt(0.25)=0.5 → step 4");
    assert_eq!(wave_glyph_index(0.01), 0, "sqrt(0.01)=0.1 → step 0");
    assert_eq!(wave_glyph_index(0.04), 1, "sqrt(0.04)=0.2 → step 1");
    // Total + monotone over a sweep; out-of-range clamps.
    let mut last = 0;
    for step in 0..=100 {
        let index = wave_glyph_index(step as f32 / 100.0);
        assert!(index < 8);
        assert!(index >= last, "monotone in level");
        last = index;
    }
    assert_eq!(wave_glyph_index(-1.0), 0);
    assert_eq!(wave_glyph_index(2.0), 7);
    assert_eq!(wave_glyph_index(f32::NAN.clamp(0.0, 1.0)), 0);
}

/// MUTATION CHECK: point the plain table at the block glyphs. Expected
/// failure: the fallback style stops being ASCII.
#[test]
fn plain_fallback_keeps_the_same_indices_in_ascii() {
    for index in 0..8 {
        let cell = WaveCell {
            glyph: index,
            hot: false,
        };
        assert_eq!(wave_glyph(cell, false), WAVE_GLYPHS_BLOCKS[index]);
        assert_eq!(wave_glyph(cell, true), WAVE_GLYPHS_PLAIN[index]);
        assert!(WAVE_GLYPHS_PLAIN[index].is_ascii());
    }
    // Out-of-range indices clamp instead of panicking.
    assert_eq!(
        wave_glyph(
            WaveCell {
                glyph: 99,
                hot: true
            },
            false
        ),
        WAVE_GLYPHS_BLOCKS[7]
    );
}

/// MUTATION CHECK: invert the hot comparison. Expected failure: the loud
/// column reads faint and the quiet one gold.
#[test]
fn hot_columns_split_at_the_speaking_threshold() {
    let mut ring = WaveRing::new();
    ring.push(0.0);
    for _ in 0..8 {
        ring.push(1.0); // drive the tail loud
    }
    let cells = ring.cells();
    let newest = cells[WAVE_WIDTH - 1];
    let oldest = cells[0];
    assert!(newest.hot, "the speaking tail wears the gold slot");
    assert!(!oldest.hot, "the silent history stays faint");
    let levels = ring.levels();
    for (cell, level) in cells.iter().zip(levels.iter()) {
        assert_eq!(cell.hot, *level >= WAVE_HOT_LEVEL);
    }
}

/// MUTATION CHECK: drop the wave spans from `chip_fit` (chip alone).
/// Expected failure: no block glyph renders on the composer band while
/// listening, although envelopes flowed.
#[test]
fn wave_renders_on_the_composer_band_next_to_the_chip() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 0.0,
    });
    for _ in 0..8 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: 1.0,
        });
    }
    let screen = draw(&model, 100, 14);
    // The status bar shows `◉ listening…` too — the BAND row is the one
    // carrying wave glyphs beside the chip.
    let band_row = screen
        .lines()
        .filter(|line| line.contains("◉ listening…"))
        .find(|line| line.chars().any(|c| WAVE_GLYPHS_BLOCKS[1..].contains(&c)))
        .expect("a listening row carries the wave");
    let chip_at = band_row.find("◉ listening…").expect("chip text located");
    let before_chip: String = band_row[..chip_at].chars().collect();
    let glyphs: Vec<char> = before_chip
        .chars()
        .filter(|c| WAVE_GLYPHS_BLOCKS.contains(c))
        .collect();
    assert_eq!(
        glyphs.len(),
        WAVE_WIDTH,
        "the full fixed-width wave renders left of the chip: {band_row:?}"
    );
    // Right-to-left law on screen: the newest (rightmost) column is the
    // loudest; the oldest history at the left sits lower.
    let index_of = |c: char| {
        WAVE_GLYPHS_BLOCKS
            .iter()
            .position(|g| *g == c)
            .expect("wave glyph")
    };
    let first = index_of(glyphs[0]);
    let last = index_of(glyphs[WAVE_WIDTH - 1]);
    assert_eq!(last, 7, "the newest column is saturated");
    assert!(first < last, "history decays toward the left edge");
}

/// MUTATION CHECK: let the wave claim the row unconditionally. Expected
/// failure: on a band too narrow for wave + chip, the chip disappears
/// (or the row overflows) instead of the wave yielding alone.
#[test]
fn wave_yields_before_the_chip_on_a_narrow_band() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 0.0,
    });
    for _ in 0..10 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: 1.0,
        });
    }
    let screen = draw(&model, 38, 12);
    let band_row = screen
        .lines()
        .find(|line| line.contains("◉ listening…"))
        .expect("the chip survives the narrow band");
    assert!(
        !band_row
            .chars()
            .any(|c| WAVE_GLYPHS_BLOCKS[1..].contains(&c)),
        "the wave yielded on the narrow band: {band_row:?}"
    );
}

/// MUTATION CHECK: feed the wave regardless of generation. Expected
/// failure: a stale envelope from a torn-down session still moves the
/// ring.
#[test]
fn stale_generation_envelopes_never_touch_the_ring() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 0.0,
    });
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 1.0,
    });
    let live = model.talk.wave.levels();
    model.handle_talk(TalkEvent::Envelope {
        generation: generation + 40,
        level: 1.0,
    });
    model.handle_talk(TalkEvent::Envelope {
        generation: generation.wrapping_sub(1),
        level: 1.0,
    });
    assert_eq!(model.talk.wave.levels(), live);
}

/// `/talk wave` flips the fallback style; the mapping indices are shared
/// so the SAME ring renders through either table.
///
/// MUTATION CHECK: make the toggle set `wave_plain = true`
/// unconditionally. Expected failure: the second toggle below does not
/// restore the block style.
#[test]
fn slash_talk_wave_toggles_the_plain_style() {
    let mut model = listening_model();
    // Cancel the session so the composer accepts the slash command.
    model.talk_cancel();
    assert!(!model.talk.wave_plain);
    common::submit(&mut model, "/talk wave");
    assert!(model.talk.wave_plain);
    common::submit(&mut model, "/talk wave");
    assert!(!model.talk.wave_plain);
}
