//! Lane voicefix (v0.0.970) — the owner's four voice-input laws:
//!
//! 1. dictation NEVER auto-sends (state machine: `t2_talk_state_tests`);
//! 2. the golden level bars track the voice in REAL TIME, and the
//!    `◉ listening…` indicator blinks at its OWN steady ~1 Hz, decoupled
//!    from audio frames;
//! 3. the animation is cheap (frame budget: `voicefix_frame_bench_tests`);
//! 4. the visualizer is HALF its old width.
//!
//! The estimator laws here are the level-tracking contract the bars draw:
//! a steady tone settles, bursts rise fast and fall slow, and silence
//! decays instead of freezing.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#![allow(clippy::float_cmp)]

use haider_tui::app::{AppModel, RuntimeMode, Screen};
use haider_tui::talk::{
    LISTEN_BLINK_PERIOD_MS, TalkEvent, TalkPhase, WAVE_ATTACK, WAVE_DECAY, WAVE_GLYPHS_BLOCKS,
    WAVE_WIDTH, WaveRing, listening_blink_on, listening_pulse_cells, wave_glyph_str,
};

mod tuivirt_common;
use tuivirt_common::{SIZES, check_golden, draw, session_model};

// ---------------------------------------------------------------------------
// 4 — the halved visualizer
// ---------------------------------------------------------------------------

/// MUTATION CHECK: put `WAVE_WIDTH` back to 24. Expected failure: the
/// visualizer reclaims the width the owner called too wide.
#[test]
fn the_visualizer_is_half_its_old_width() {
    assert_eq!(
        WAVE_WIDTH, 12,
        "970 owner requirement 4: the wave's fixed cell budget is HALVED from 24"
    );
    let ring = WaveRing::new();
    assert_eq!(
        ring.levels().len(),
        WAVE_WIDTH,
        "the ring capacity follows the rendered width — one source of truth"
    );
    assert_eq!(ring.cells_iter().len(), WAVE_WIDTH);
    assert_eq!(listening_pulse_cells(0).len(), WAVE_WIDTH);
}

// ---------------------------------------------------------------------------
// 2 — the level estimator
// ---------------------------------------------------------------------------

/// A STEADY TONE settles: once the floor is calibrated, a constant
/// louder-than-floor sample converges upward to its calibrated value and
/// stays there instead of oscillating.
///
/// MUTATION CHECK: make the attack rate 1.0. Expected failure: the first
/// sample jumps straight to the target and the "rises toward" assertions
/// below (a strictly increasing approach) collapse.
#[test]
fn a_steady_tone_converges_and_holds() {
    let mut ring = WaveRing::new();
    ring.push(0.0); // calibrate the floor at 0
    let mut seen = Vec::new();
    for _ in 0..24 {
        ring.push(0.5);
        seen.push(ring.current());
    }
    // Monotone approach, never overshooting the calibrated target.
    for pair in seen.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "a steady tone approaches monotonically: {seen:?}"
        );
        assert!(pair[1] <= 0.5 + 1e-6, "it never overshoots the tone");
    }
    assert!(
        (ring.current() - 0.5).abs() < 1e-3,
        "a steady tone settles ON the tone, not below it: {}",
        ring.current()
    );
    assert!(ring.fed(), "a fed ring reports itself fed");
}

/// BURSTS: the estimator rises fast and falls slow, so a syllable is
/// visible and the tail between syllables reads as motion rather than a
/// flicker to zero.
///
/// MUTATION CHECK: make attack and decay equal. Expected failure: the
/// burst's fall matches its rise and the asymmetry assertion breaks.
#[test]
fn bursts_rise_fast_and_fall_slow() {
    let mut ring = WaveRing::new();
    ring.push(0.0);
    // One loud burst.
    ring.push(1.0);
    let after_one_attack = ring.current();
    assert_eq!(
        after_one_attack, WAVE_ATTACK,
        "one attack step of the way up"
    );
    // …and the fall from it is the SLOW rate.
    ring.push(0.0);
    let fell = after_one_attack - ring.current();
    let rose = after_one_attack;
    assert!(
        fell < rose,
        "the fall ({fell}) must be slower than the rise ({rose})"
    );
    assert!(
        (fell - after_one_attack * WAVE_DECAY).abs() < 1e-6,
        "the fall is exactly one decay step — the asymmetry against the \
         attack step above is what makes speech read as motion"
    );
}

/// SILENCE DECAYS toward zero — the bars drain instead of freezing at the
/// last loud value when the speaker stops.
///
/// MUTATION CHECK: freeze `smoothed` when the sample is quieter. Expected
/// failure: the ring holds its peak forever and silence looks like speech.
#[test]
fn silence_decays_toward_zero() {
    let mut ring = WaveRing::new();
    ring.push(0.0);
    for _ in 0..8 {
        ring.push(1.0);
    }
    let loud = ring.current();
    assert!(loud > 0.9, "the burst got loud first: {loud}");
    let mut previous = loud;
    for _ in 0..60 {
        ring.push(0.0);
        assert!(
            ring.current() < previous,
            "every silent sample decays further: {} !< {previous}",
            ring.current()
        );
        previous = ring.current();
    }
    assert!(
        ring.current() < 0.02,
        "sustained silence drains the bars: {}",
        ring.current()
    );
    assert!(
        ring.max_level() < 0.02,
        "and drains the whole visible ring, not just the newest column"
    );
}

// ---------------------------------------------------------------------------
// 2 — the blink is decoupled from the audio
// ---------------------------------------------------------------------------

/// The `◉ listening…` blink runs on the WALL CLOCK at ~1 Hz: level ticks
/// cannot advance it, and it does not need them to keep blinking.
///
/// MUTATION CHECK: drive the blink from `anim_phase` (or from the
/// envelope count) again. Expected failure: either a burst of envelopes
/// changes the blink state at a fixed clock, or the blink stops moving
/// when the mic goes quiet.
#[test]
fn the_blink_is_one_hz_and_independent_of_level_ticks() {
    // One full cycle per second, lit for the first half.
    assert_eq!(LISTEN_BLINK_PERIOD_MS, 1_000);
    assert!(listening_blink_on(0));
    assert!(listening_blink_on(499));
    assert!(!listening_blink_on(500));
    assert!(!listening_blink_on(999));
    assert!(listening_blink_on(1_000), "it repeats every second");
    assert!(!listening_blink_on(12_750), "and keeps its phase far out");

    // A THOUSAND level ticks at a frozen clock cannot move the blink.
    let mut model = listening_model();
    model.clock_ms = 250; // lit half
    let generation = model.talk.generation;
    let lit_before = listening_blink_on(model.clock_ms);
    for n in 0..1_000 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: if n % 3 == 0 { 0.9 } else { 0.01 },
        });
    }
    assert_eq!(
        listening_blink_on(model.clock_ms),
        lit_before,
        "audio frames must NOT drive the blink"
    );
    assert!(
        model.talk.wave.fed(),
        "…even though those frames genuinely moved the bars"
    );

    // And the clock alone flips it, with no audio at all.
    model.clock_ms += LISTEN_BLINK_PERIOD_MS / 2;
    assert_eq!(
        listening_blink_on(model.clock_ms),
        !lit_before,
        "the wall clock alone drives the blink"
    );
}

/// …and the RENDER actually consults that clock. The law above is a pure
/// function; this pins the chip's ink to it, so the blink is wired rather
/// than merely available.
///
/// MUTATION CHECK: render the listening chip from `anim_phase` again.
/// Expected failure: `anim_phase` is 0 across both frames below, so the
/// two clock phases render the SAME ink and the inequality breaks.
#[test]
fn the_rendered_listening_chip_follows_the_blink_clock() {
    let ink_at = |clock_ms: u64| {
        let mut model = listening_model();
        model.clock_ms = clock_ms;
        assert_eq!(model.anim_phase, 0, "the phase counter is NOT the source");
        let frame = draw(&model, 118, 36);
        let row = frame
            .row_containing("◉ listening…")
            .expect("the listening chip renders");
        // Cell COLUMN, not a byte offset — the row is UTF-8 and the grid
        // is indexed by cell.
        let column = frame.cells[row]
            .iter()
            .position(|(symbol, _)| symbol == "◉")
            .expect("the chip's dot locates the styled run");
        frame.cells[row][column].1
    };
    // 250 ms is the lit half of the cycle, 750 ms the dim half.
    let lit = ink_at(250);
    let dim = ink_at(750);
    assert!(
        listening_blink_on(250) && !listening_blink_on(750),
        "the two samples straddle a blink edge"
    );
    assert_ne!(
        lit, dim,
        "the listening chip must actually change ink across the blink edge"
    );
}

// ---------------------------------------------------------------------------
// 2 — the bars are live, not a canned sweep
// ---------------------------------------------------------------------------

/// The owner's "frozen/late" bug: the render used to fall back to the
/// synthesized sweep whenever the ring's peak dipped below a loudness
/// threshold — which is every pause between words — so mid-sentence the
/// bars stopped showing the voice and started stepping at the 600 ms
/// phase clock. Once a mic has fed the ring, the ring IS the display.
///
/// MUTATION CHECK: restore the `max_level() >= LISTENING_SIGNAL_MIN`
/// fallback test. Expected failure: the quiet-but-fed ring below renders
/// the canned sweep's shape instead of its own near-silent bars.
#[test]
fn a_fed_ring_keeps_drawing_real_audio_through_a_quiet_passage() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    // One quiet sample first, so the session noise floor calibrates at 0
    // and the speech that follows reads as real headroom above it.
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 0.0,
    });
    // Speech, then a pause quiet enough to sit under the old threshold.
    for _ in 0..6 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: 0.8,
        });
    }
    for _ in 0..40 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: 0.0,
        });
    }
    assert!(model.talk.wave.fed());
    assert!(
        model.talk.wave.max_level() < 0.03,
        "the pause really is below the old signal threshold: {}",
        model.talk.wave.max_level()
    );
    // The canned sweep always paints a saturated crest somewhere; a
    // genuinely quiet ring must not.
    let sweep_has_crest = listening_pulse_cells(model.clock_ms)
        .iter()
        .any(|cell| cell.glyph >= 5);
    assert!(sweep_has_crest, "the synthesized sweep is a loud shape");
    let quiet: Vec<usize> = model
        .talk
        .wave
        .cells_iter()
        .map(|cell| cell.glyph)
        .collect();
    assert!(
        quiet.iter().all(|glyph| *glyph == 0),
        "a quiet passage draws quiet — no canned crest: {quiet:?}"
    );

    // An UNFED ring is the one honest use of the sweep: the mic is open
    // but no level has arrived, so the row animates instead of flatlining.
    let fresh = listening_model();
    assert!(!fresh.talk.wave.fed());
}

/// The rendered symbols are BORROWED, never minted per cell (970 owner
/// requirement 3: no allocation on the animation path).
///
/// MUTATION CHECK: return `String` from the glyph accessor. Expected
/// failure: this no longer compiles against a `&'static str` binding.
#[test]
fn wave_glyphs_are_borrowed_not_allocated() {
    let ring = {
        let mut ring = WaveRing::new();
        ring.push(0.0);
        for _ in 0..8 {
            ring.push(1.0);
        }
        ring
    };
    for cell in ring.cells_iter() {
        let symbol: &'static str = wave_glyph_str(cell, false);
        assert_eq!(symbol.chars().count(), 1);
        assert!(WAVE_GLYPHS_BLOCKS.contains(&symbol.chars().next().expect("one char")));
        let plain: &'static str = wave_glyph_str(cell, true);
        assert!(plain.is_ascii(), "the plain ramp stays ASCII");
    }
}

// ---------------------------------------------------------------------------
// Golden frames of the listening row
// ---------------------------------------------------------------------------

/// A live session on the session screen with `/talk` LISTENING.
fn listening_model() -> AppModel {
    let mut model = session_model();
    model.mode = RuntimeMode::Live;
    assert_eq!(model.screen, Screen::Session);
    model.talk_toggle();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Started {
        generation,
        sample_rate: 48_000,
    });
    assert_eq!(model.talk.phase, TalkPhase::Listening);
    model.requests.clear();
    // Pin the render clock so the blink phase and the sweep are
    // deterministic in every golden.
    model.clock_ms = 250;
    model
}

/// IDLE — listening, mic open, no level has arrived yet: the synthesized
/// sweep animates the row and the blink is on its lit half.
#[test]
fn golden_listening_row_idle() {
    let model = listening_model();
    assert!(!model.talk.wave.fed());
    for (width, height) in SIZES {
        check_golden("talk_listening_idle", &draw(&model, width, height));
    }
}

/// SPEAKING — real envelopes have fed the ring, so the row carries the
/// live golden bars at their halved width.
#[test]
fn golden_listening_row_speaking() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    model.handle_talk(TalkEvent::Envelope {
        generation,
        level: 0.0,
    });
    for step in 0..10 {
        model.handle_talk(TalkEvent::Envelope {
            generation,
            level: if step % 2 == 0 { 0.85 } else { 0.35 },
        });
    }
    model.handle_talk(TalkEvent::Partial {
        generation,
        frame: haider_stt::TranscriptFrame {
            provider: haider_stt::EngineKind::WhisperLocal,
            text: "the quick brown fox".to_owned(),
            is_final: false,
            speech_final: false,
        },
    });
    assert!(model.talk.wave.fed());
    for (width, height) in SIZES {
        check_golden("talk_listening_speaking", &draw(&model, width, height));
    }
}

/// STOPPED — the session ended: no wave, no listening chip, and the
/// transcript sitting in the composer awaiting ⏎ (owner requirement 1).
#[test]
fn golden_listening_row_stopped() {
    let mut model = listening_model();
    let generation = model.talk.generation;
    model.talk_toggle();
    model.handle_talk(TalkEvent::Finished {
        generation,
        result: Ok(haider_stt::TranscriptionResult {
            text: "the quick brown fox".to_owned(),
            segments: 1,
            duration_ms: 2400,
        }),
    });
    assert_eq!(model.talk.phase, TalkPhase::Idle);
    assert!(!model.listening);
    assert_eq!(model.composer.text(), "the quick brown fox ");
    for (width, height) in SIZES {
        check_golden("talk_listening_stopped", &draw(&model, width, height));
    }
}
