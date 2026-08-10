//! W-E — the animated left→right shimmer on the Thinking status verb. A
//! highlight window of three display cells sweeps across the word `thinking`
//! on the shared `anim_phase` clock: the centre glyph lifts to the emphasis
//! ink, its ±1 shoulders to a gold→emphasis midpoint, everything else rests
//! at the gold accent. The dot keeps its uniform pulse + ● ↔ ◌ breath and
//! the trailing `…` is static base ink — the moving part is the verb alone.
//!
//! The laws (brief LE1–LE7):
//!   LE1 phase purity — `intensity(tick, index)` is a pure function.
//!   LE2 sweep travels — the bright centre advances left→right and wraps.
//!   LE3 scope — only the verb's glyphs vary; the `…` (and the dot) do not.
//!   LE4 idle — with no run in flight, ticks render byte-identical frames.
//!   LE5 theme sweep — the three inks are tokens clearing the contrast floor.
//!   LE6 degradation — non-truecolor collapses to a two-tone wave, no mid.
//!   LE7 plain — the `--plain` path never carries the shimmer.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use haider_protocol::EventPayload;
use haider_protocol::state::RunState;
use haider_tui::app::{AppEvent, AppModel, Screen};
use haider_tui::plain::render_plain;
use haider_tui::render::render;
use haider_tui::runtime::truecolor_capable;
use haider_tui::style::{SHIMMER_TAIL, shimmer_centre, shimmer_level};
use haider_tui::theme::{Rgb, ThemeKey};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

mod common;
use common::{hit_session_named, launcher_model};

/// The status verb the shimmer sweeps (kept in lockstep with `render.rs`).
const VERB: &str = "thinking";

fn draw(model: &AppModel, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render(model, frame);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    (rows, terminal)
}

fn row_of(rows: &[String], needle: &str) -> u16 {
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("row not found: {needle}")),
    )
    .expect("row index")
}

fn col_of(row: &str, needle: &str) -> usize {
    let byte = row
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} in {row:?}"));
    row[..byte].chars().count()
}

/// The rendered inks of the thinking tail: each verb glyph, the trailing
/// `…`, and the leading dot — read straight off the cells (raw fg, not the
/// glyph text) so the assertions see the SGR truth the probe ladder greps.
struct TailInks {
    letters: Vec<Color>,
    ellipsis: Color,
    dot: Color,
}

fn tail_inks(model: &AppModel) -> TailInks {
    let (rows, terminal) = draw(model, 118, 34);
    let y = row_of(&rows, "thinking…");
    let row = &rows[y as usize];
    let start = col_of(row, VERB);
    assert!(start >= 2, "the ` <dot> ` prefix sits left of the verb");
    let buffer = terminal.backend().buffer();
    let len = VERB.chars().count();
    let letters = (0..len)
        .map(|i| buffer[(u16::try_from(start + i).unwrap(), y)].fg)
        .collect();
    let ellipsis = buffer[(u16::try_from(start + len).unwrap(), y)].fg;
    // The dot glyph (`● `/`◌ `) is the two cells immediately left of the verb.
    let dot = buffer[(u16::try_from(start - 2).unwrap(), y)].fg;
    TailInks {
        letters,
        ellipsis,
        dot,
    }
}

/// A session paused mid-turn: `is_thinking()` true, so the session tail
/// renders the shimmering verb.
fn thinking_model() -> AppModel {
    let mut model = launcher_model();
    hit_session_named(&mut model, "billing-service");
    model.handle(AppEvent::Envelope(Box::new(EventPayload::RunState(
        RunState::Thinking,
    ))));
    model
}

// WCAG relative luminance / contrast — the theme suite's oracle, copied
// verbatim (an independent check, per the suite's convention).
fn luminance(rgb: Rgb) -> f64 {
    fn channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---- LE1: the shimmer phase is a pure function ----

#[test]
fn le1_shimmer_phase_is_a_pure_function() {
    // Same (phase, index, len) → same level, every time and independent of
    // call order: a stateless, reproducible frame for the probe ladder.
    let len = VERB.chars().count();
    for phase in 0u8..=64 {
        for index in 0..len {
            let a = shimmer_level(phase, index, len);
            let b = shimmer_level(phase, index, len);
            assert_eq!(a, b, "level({phase},{index}) is deterministic");
            assert!(a <= 2, "levels are base/mid/bright only");
        }
    }
    // The rendered tail at a FIXED tick reproduces itself byte-for-byte.
    let mut model = thinking_model();
    model.anim_phase = 5;
    let first = tail_inks(&model).letters;
    model.anim_phase = 5;
    let again = tail_inks(&model).letters;
    assert_eq!(first, again, "a fixed tick renders one frame, always");
}

// ---- LE2: the sweep travels left→right and wraps ----

#[test]
fn le2_the_sweep_travels_and_wraps() {
    let len = VERB.chars().count();
    // The pure centre sequence: 0, 1, … len-1, then a tail rest, then back
    // to 0. A static "always bright at 0" cannot produce this ordering.
    let period = len + SHIMMER_TAIL;
    let seq: Vec<Option<usize>> = (0..period)
        .map(|p| shimmer_centre(u8::try_from(p).unwrap(), len))
        .collect();
    let expected: Vec<Option<usize>> = (0..len)
        .map(Some)
        .chain(std::iter::repeat_n(None, SHIMMER_TAIL))
        .collect();
    assert_eq!(seq, expected, "one glyph per tick, then a short tail rest");
    assert_eq!(
        shimmer_centre(u8::try_from(period).unwrap(), len),
        Some(0),
        "the sweep wraps after the tail"
    );
    assert_ne!(
        shimmer_centre(0, len),
        shimmer_centre(1, len),
        "consecutive ticks move the centre — it is not pinned"
    );

    // Rendered: the BRIGHT verb cell advances 0 → 1 → 2 across the wire.
    let mut model = thinking_model();
    let bright = Color::from(model.theme.theme().shimmer_inks()[2]);
    let bright_col = |model: &AppModel| tail_inks(model).letters.iter().position(|c| *c == bright);
    model.anim_phase = 0;
    let c0 = bright_col(&model);
    model.anim_phase = 1;
    let c1 = bright_col(&model);
    model.anim_phase = 2;
    let c2 = bright_col(&model);
    assert_eq!(c0, Some(0), "phase 0: the wave crests on the first glyph");
    assert_eq!(c1, Some(1));
    assert_eq!(c2, Some(2));
    assert!(c0 < c1 && c1 < c2, "the bright crest travels left→right");
}

// ---- LE3: only the verb's glyphs vary ----

#[test]
fn le3_only_the_verb_glyphs_shimmer() {
    let mut model = thinking_model();
    let theme = model.theme.theme();
    let base = Color::from(theme.shimmer_inks()[0]);
    let bright = Color::from(theme.shimmer_inks()[2]);
    let pulse_off = Color::from(theme.gold.over(theme.bg, 350));
    let period = u8::try_from(VERB.chars().count() + SHIMMER_TAIL).unwrap();

    // The trailing `…` carries BASE ink in EVERY frame — never lifted by the
    // wave. (Fold the `…` into the shimmer loop and this fails.)
    for phase in 0..=(period + 1) {
        model.anim_phase = phase;
        assert_eq!(
            tail_inks(&model).ellipsis,
            base,
            "the `…` never shimmers (phase {phase})"
        );
    }

    // The dot stays as-is: its OFF phase is the uniform pulse dim, not any
    // shimmer ink — the dot is not routed through the wave.
    model.anim_phase = 1;
    assert_eq!(
        tail_inks(&model).dot,
        pulse_off,
        "the dot keeps its uniform pulse (● ↔ ◌), untouched by the shimmer"
    );

    // The verb itself DOES move: some glyph reaches bright across the sweep.
    let mut verb_varies = false;
    for phase in 0..VERB.chars().count() {
        model.anim_phase = u8::try_from(phase).unwrap();
        if tail_inks(&model).letters.contains(&bright) {
            verb_varies = true;
        }
    }
    assert!(verb_varies, "the verb's glyphs carry the moving accent");
}

// ---- LE4: idle costs nothing ----

#[test]
fn le4_idle_status_line_is_byte_identical_across_ticks() {
    // A fully idle session: no thinking, no tools, no todos, no chips —
    // nothing animates, so different ticks MUST render identical bytes and
    // the runtime clock stays parked (zero idle wakeups).
    let mut model = launcher_model();
    for session in &mut model.sessions {
        session.chips.clear();
    }
    hit_session_named(&mut model, "billing-service");
    assert_eq!(model.screen, Screen::Session);
    assert!(!model.animated(), "the idle session requests no repaint");

    model.anim_phase = 0;
    let (frame_a, _) = draw(&model, 118, 34);
    model.anim_phase = 137;
    let (frame_b, _) = draw(&model, 118, 34);
    assert_eq!(
        frame_a, frame_b,
        "idle frames are phase-invariant — the shimmer costs nothing at rest"
    );
    assert!(
        !frame_a.iter().any(|row| row.contains("thinking…")),
        "an idle session shows no thinking tail at all"
    );
}

// ---- LE5: every theme's three inks are legible tokens ----

#[test]
fn le5_every_theme_shimmer_ink_clears_the_contrast_floor() {
    for key in ThemeKey::ALL {
        let theme = key.theme();
        let [base, mid, bright] = theme.shimmer_inks();
        // The three inks come from theme tokens — base is the gold accent,
        // bright the emphasis ink, verbatim (no invented color).
        assert_eq!(base, theme.gold, "{}: base is the gold token", theme.label);
        assert_eq!(
            bright, theme.bright,
            "{}: bright is the emphasis token",
            theme.label
        );
        // Each clears the accent contrast floor (3.2:1) on its own ground.
        for (name, ink) in [("base", base), ("mid", mid), ("bright", bright)] {
            let ratio = contrast(ink, theme.bg);
            assert!(
                ratio >= 3.2,
                "{}: shimmer {name} contrast {ratio:.2} under the 3.2 floor",
                theme.label
            );
        }
        // A real brightness ladder: strictly more prominent base → bright,
        // so the wave lifts (never a flat or inverted sweep).
        assert!(
            contrast(base, theme.bg) < contrast(mid, theme.bg),
            "{}: mid must out-contrast base",
            theme.label
        );
        assert!(
            contrast(mid, theme.bg) < contrast(bright, theme.bg),
            "{}: bright must out-contrast mid",
            theme.label
        );
    }
}

// ---- LE6: non-truecolor degrades to a two-tone wave ----

#[test]
fn le6_non_truecolor_degrades_to_two_tone_without_the_mid_code() {
    // The capability detector defaults to truecolor and downgrades only on
    // positive 16-color evidence.
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    };
    assert!(truecolor_capable(&env(&[("COLORTERM", "truecolor")])));
    assert!(truecolor_capable(&env(&[("COLORTERM", "24bit")])));
    assert!(truecolor_capable(&env(&[("TERM", "xterm-256color")])));
    assert!(
        truecolor_capable(&env(&[])),
        "absent env defaults to truecolor"
    );
    assert!(!truecolor_capable(&env(&[("TERM", "linux")])));
    assert!(!truecolor_capable(&env(&[("TERM", "xterm-16color")])));
    assert!(
        truecolor_capable(&env(&[("COLORTERM", "truecolor"), ("TERM", "linux")])),
        "an explicit truecolor COLORTERM overrides a 16-color TERM"
    );

    let mut model = thinking_model();
    let base = Color::from(model.theme.theme().shimmer_inks()[0]);
    let mid = Color::from(model.theme.theme().shimmer_inks()[1]);
    let bright = Color::from(model.theme.theme().shimmer_inks()[2]);
    let sweep = u8::try_from(VERB.chars().count()).unwrap();

    // Two-tone: across the whole sweep the verb only ever wears base or
    // bright — the MID shoulder code never crosses the wire.
    model.truecolor = false;
    let (mut saw_base, mut saw_bright) = (false, false);
    for phase in 0..sweep {
        model.anim_phase = phase;
        for ink in tail_inks(&model).letters {
            assert!(
                ink == base || ink == bright,
                "degraded mode emits no intermediate (mid) truecolor code"
            );
            saw_base |= ink == base;
            saw_bright |= ink == bright;
        }
    }
    assert!(
        saw_base && saw_bright,
        "the two-tone wave still travels (bright crest over a base word)"
    );

    // Truecolor: the mid shoulder DOES appear — the fuller falloff.
    model.truecolor = true;
    let mut saw_mid = false;
    for phase in 0..sweep {
        model.anim_phase = phase;
        if tail_inks(&model).letters.contains(&mid) {
            saw_mid = true;
        }
    }
    assert!(saw_mid, "truecolor keeps the mid shoulder in the falloff");
}

// ---- LE7: plain output is unchanged ----

#[test]
fn le7_plain_mode_carries_no_shimmer() {
    // The `--plain` path takes no phase and emits no color: it cannot
    // animate, and the shimmer never leaks into the pipe/CI oracle.
    let mut model = thinking_model();
    model.anim_phase = 0;
    let at_zero = render_plain(&model.projection, 80, None);
    model.anim_phase = 200;
    let at_tick = render_plain(&model.projection, 80, None);
    assert_eq!(at_zero, at_tick, "plain output is clock-invariant");
    assert!(
        !at_zero.contains('\u{1b}'),
        "plain output carries no escape bytes"
    );
}
