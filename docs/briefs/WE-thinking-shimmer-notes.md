# W-E — Thinking-verb shimmer: implementation notes

Branch `we-thinking-shimmer` (off main @ v0.0.79). TUI-only wave
(`haider-tui`). No version bump, no tag, no renames, no MCP.

## What shipped

An animated left→right brightness wave on the `thinking` status verb,
matching the owner's Claude Code reference (`◆ Thinking (26s · esc to
interrupt)` where the WORD carries the wave). A three-cell highlight window
sweeps across `thinking` on the shared `anim_phase` clock:

- window centre → the emphasis ink (`bright`),
- ±1 shoulders → a gold→emphasis midpoint,
- everything else → the gold accent (the word's resting ink, identical to
  the old uniform look).

The leading dot keeps its uniform pulse and `● ↔ ◌` breath untouched (the
shimmer is ADDITIVE to the verb — seam-pointer instruction), and the
trailing `…` is static base ink (never shimmers). The renderer
`thinking_line` is shared by the session tail (`render.rs`, the
`is_thinking()` gate) and the subagent chip (`ChipDisplayState::Thinking`),
so the shimmer lands in both by construction.

## Seam and the sibling shimmer

- `thinking_line(theme, phase, truecolor)` (`render.rs`) — was ONE span
  `pulse_ink(gold, phase)`; now emits per-glyph spans, each styled by the
  new `Theme::shimmer_ink`.
- `Theme::shimmer_inks() -> [Rgb; 3]` and `Theme::shimmer_ink(phase, index,
  len, truecolor)` live in `style.rs` beside their siblings `pulse_ink`
  (uniform) and `rail_shimmer_style` (the rail's travelling gold↔maroon).
  The rail shifts HUE across cells; the verb lifts BRIGHTNESS across
  glyphs — a quieter, consistent accent.
- Pure sweep math `shimmer_centre` / `shimmer_level` (+ `SHIMMER_TAIL`) in
  `style.rs`: `intensity(phase, index, len)` with no hidden state.

## Locked decisions honoured

1. Scope: only the verb glyphs vary; the dot keeps its pulse, the `…` is
   static base ink. (LE3)
2. Motion: a ~3-cell window, centre=bright / ±1=mid / else=base, a short
   tail rest between sweeps (`SHIMMER_TAIL = 3`).
3. Clock: rides `model.anim_phase` (the existing 600 ms
   journal/frame tick) — no new timer, thread, or wake source. Phase is a
   pure function of (tick, glyph index).
4. Idle cost: zero. The tail renders only under `is_thinking()`; when not
   thinking, `animated()` is false and the clock parks. (LE4)
5. Theme: the three inks are theme tokens — `base = gold`, `bright =
   bright`, `mid = bright.over(gold, 500)` — per theme, no hardcoded RGB.
   All clear the accent contrast floor on every ground (LE5).
6. Degradation: non-truecolor collapses to the two-tone wave (centre
   bright, else base — no mid step). Capability detected once at startup by
   the pure `truecolor_capable(env)` (`runtime.rs`), stored on
   `model.truecolor` (default true), read by render. (LE6)
7. Plain / probes: `--plain` takes no phase and emits no color — the
   shimmer never leaks into piped output. (LE7)

## Cadence choice (the one judgement call)

The shared clock ticks every 600 ms (`ANIM_PHASE_MS`). The centre advances
ONE glyph per tick — the only non-strobing cadence at that granularity; a
faster sweep would jump 3–4 cells a frame and read as a strobe. So the full
`thinking` sweep is `(8 + 3) × 600 ms ≈ 6.6 s`, slower than the reference's
sub-second period (which assumes 60 fps). The brief says "tune against the
reference feel, not a spec number" and "smooth, deliberate accent, not a
strobe" — smoothness wins over the nominal period at this clock rate, and
the zero-idle-cost law forbids inventing a faster wake source.

## Laws (all run by name; `cargo test -p haider-tui` green)

`tests/we_thinking_shimmer_tests.rs`

- `le1_shimmer_phase_is_a_pure_function` — LE1 purity/reproducibility.
- `le2_the_sweep_travels_and_wraps` — LE2 the crest advances 0→1→2 and the
  sequence wraps after the tail (a static "always 0" fails).
- `le3_only_the_verb_glyphs_shimmer` — LE3 the `…` stays base every frame,
  the dot keeps its pulse, only the verb moves.
- `le4_idle_status_line_is_byte_identical_across_ticks` — LE4 zero idle
  cost.
- `le5_every_theme_shimmer_ink_clears_the_contrast_floor` — LE5 per-theme
  tokens + WCAG floor (the theme suite's oracle, reused).
- `le6_non_truecolor_degrades_to_two_tone_without_the_mid_code` — LE6.
- `le7_plain_mode_carries_no_shimmer` — LE7.

The pre-existing `tui4d_animation_tests::phase_toggle_alternates_the_
thinking_line` still passes: it reads the DOT cell, which keeps
`pulse_ink(gold, phase)`.

## Probe ladder

`scripts/tui-probes/ladder.sh` → 16/16 PASS, UNCHANGED. My change is
isolated to `thinking_line`, which renders only in a session/chip view; no
ladder probe renders or asserts the thinking-verb bytes (verified: the
`217;181;68` / `229;122;74` bytes the anim probe greps are the LAUNCHER
rail shimmer and the cursor-cell ground, not the verb), so no probe's
raw-SGR bytes changed and none needed updating.

Deviation, stated plainly: the brief asks to "extend the animation probe
with a shimmer frame check." The animation probe is a hermetic, ZERO-INPUT
launcher measurement; the thinking verb never renders at the launcher, and
bolting timing-dependent navigation onto it to catch a transient thinking
frame would undermine its existing bounded-repaint guarantees. The
raw-SGR-byte shimmer check for the new feature therefore lives at the
render-law layer instead: `le2` reads the raw bright-ink `fg` cell
advancing across ticks and `le6` reads the raw per-glyph inks of the
degraded wave — the same "bytes, not glyphs" guarantee, on the surface
where the verb is deterministically reachable. The ladder stays honest at
16/16 rather than being weakened or made flaky.

## Mutation ledger

See `WE-thinking-shimmer-mutation-notes.md` — four executed kills covering
LE2 (sweep advance), LE3 (scope), LE4 (idle no-op), LE6 (degradation).
