# W-E — animated left-to-right shimmer on the Thinking indicator

Owner contract (with screenshot): "also support this animated left to
right version of thinking". Reference: Claude Code renders
`◆ Thinking (26s · esc to interrupt)` where the WORD ITSELF carries a
brightness wave travelling left → right — leading characters brighten,
trailing characters fall back to the base ink, continuously looping
while the turn is in flight.

## Locked design decisions

1. SCOPE: the shimmer applies to the STATUS VERB only (the word that
   names the current state — Thinking / Working / Compacting / etc. as
   the existing status line renders it), never to the elapsed timer, the
   separator, or the `esc to interrupt` hint. Those keep their current
   dim ink so the moving part reads as one deliberate accent.
2. MOTION MODEL: a highlight window of ~3-4 display cells sweeps from
   the first glyph to the last, then repeats after a short tail pause
   (so it reads as a pulse, not a strobe). Per-character intensity is a
   falloff from the window centre: centre = bright ink, ±1 = mid, beyond
   = base ink. Period ~1.1-1.4 s end to end; tune against the reference
   feel, not a spec number.
3. CLOCK: ride the EXISTING TUI animation clock (the same
   journal/frame tick the breathing rows and S4 elapsed columns use) —
   do NOT add a new timer, thread, or wake source. The phase is a pure
   function of (tick, glyph index), so the renderer stays stateless and
   frames remain reproducible for probes.
4. IDLE COST: zero. When no run is in flight the status line does not
   animate and requests no repaints — pin this (a law that asserts the
   idle frame is byte-identical across ticks).
5. THEME: the bright/mid/base inks come from the existing theme tokens
   (the status/dim family), per theme, never hardcoded RGB. Light and
   dark must both stay legible; the shimmer raises brightness within
   the palette rather than inventing a new hue.
6. DEGRADATION: on terminals without truecolor, collapse to the
   existing two-tone (bright vs base) treatment — the wave still moves,
   just with fewer steps. Never emit SGR the terminal cannot render.
   Respect any existing reduced-motion/no-animation setting the TUI
   already honours; if none exists, do NOT invent a setting this wave.
7. PLAIN MODE / probes: plain (non-TTY) output is unchanged — no
   animation, no escape soup in piped output. Pin that.

## Mandatory laws

- LE1 phase purity: intensity(tick, index) is a pure function; the same
  tick yields the same frame (reproducibility for the probe ladder).
- LE2 sweep travels: across successive ticks the bright centre index
  increases and wraps — assert the SEQUENCE, not one frame (a static
  "always bright at 0" implementation must fail).
- LE3 scope: the timer and `esc to interrupt` hint carry base ink in
  every frame; only the verb's glyphs vary.
- LE4 idle: with no run in flight, two different ticks render
  byte-identical status lines.
- LE5 theme sweep: for every ThemeKey, the three inks come from theme
  tokens and satisfy the existing contrast floors (reuse the WCAG law
  helper from the theme suite).
- LE6 non-truecolor degradation renders the two-tone variant and emits
  no truecolor SGR.
- LE7 plain mode unchanged (existing plain fixtures stay green).
- Probe ladder: extend the animation probe with a shimmer frame check
  (raw SGR bytes — the ladder greps bytes, not glyphs).

## Discipline

TUI-only wave (haider-tui). Standard rules: CARGO_INCREMENTAL=0;
`cargo test -p haider-tui` green per commit; `cargo fmt --all -- --check`
clean; named-path adds; ledger truthful; notes + mutation-notes with ≥4
EXECUTED kills covering: the sweep advance (LE2), the scope restriction
(LE3), the idle no-op (LE4), and the degradation path (LE6). Run
`scripts/tui-probes/ladder.sh` BEFORE the wave is called done (standing
ritual for any TUI-visual change). No version bumps/tags/renames.
