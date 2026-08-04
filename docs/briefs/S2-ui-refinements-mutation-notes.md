# S2 UI-refinements mutation notes

Owner-directed refinement wave (screenshots): pearl-white LIGHT, Codex-register
neutral DARK, the 24×2 header-mark rebuild, the one-line composer rest height,
the transcript breathing rhythm, and the child chip view's user rows. Landed on
`s2-ui-refinements` in four milestones:

1. `theme: S2 light/dark redesign` — LIGHT re-grounded on pearl `#fdfdfd`
   (neutral near-black ink, neutral grays, gold deepened to `#7d5a05`); DARK
   re-grounded on neutral charcoal `#0f0f0f` (neutral `#d4d4d4` text, cool
   `#8b949e` dim, every chrome blend mixed from WHITE); gold `#d9b544` and
   ember `#e57a4a` kept byte-identical as the restrained identity accents.
   Probe-ladder SGR constants track the new dark bytes: selBg `39;39;39`,
   barBg `25;25;25` (gold/ember rows unchanged).
2. `mark: rebuild the header حيدر at 24×2` — the 16-col map read as
   disconnected squares (owner screenshot); the rebuild is the 28×8 boot
   banner at HALF vertical resolution (each 2-px stroke pair → one pixel row)
   tightened 28 → 24 by closing inter-letter air only.
3. `render: composer band rests at one line; breathing rhythm; chip user rows`
   — the TUI6-era `band_pad` row retired on session + subagent; one trailing
   blank line in the transcript STREAM before the band; one blank line above
   the thinking badge (session + chip views); chip-scoped user messages pinned
   end-to-end through `route_raw`/`classify`.
4. `ledger: 1578` (+6 pins, `xtask test-count --update`).

Palettes were designed against a WCAG-luminance harness BEFORE the hexes were
committed (both clear every floor of `theme_tests::
every_theme_clears_wcag_contrast_floors` and `ui_themes_tests::
every_theme_clears_the_contrast_floors` on first principles, not test-tuning).
Probe ladder run locally on the release binaries per the v0.0.62 ritual:
**16/16 PASS (14 demo + 2 live)**.

## Executed mutations

Every mutation below was EXECUTED on 2026-08-04 against the committed tree
(apply via single-anchor python assert → run the ONE named observer, requiring
`running 1 test` → observe the RUNTIME failure → `git checkout` revert).
Observers live in `haider-tui/tests/s2_ui_refinement_tests.rs` unless another
file is named.

| Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|
| `composer_height` clamp floor 1 → 2 (the band rests two rows — the owner's exact defect, equivalent to restoring `band_pad`). | `composer_rests_at_one_line_and_grows` | "closing rule directly under the rest composer, got \"…blank row…\"" — the padded band puts a blank where the rule must be. |
| `composer_height` clamp cap → 1 (the band never grows). | `composer_rests_at_one_line_and_grows` | `row containing "❯ a" not rendered` — the one-row band tail-windows the two-line draft down to its cursor row, hiding the first draft row the growth law demands. |
| The session transcript's trailing blank gated `if false`. | `one_blank_line_before_composer_band` | "one blank line between the last output and the band, got \" ❯ prompt 11…\"" — the tail sits flush on the gold rule (the cramped screenshot). |
| The session thinking badge's lead blank dropped. | `one_blank_line_above_thinking_badge` | "session: one blank line above the badge, got \" ❯ prompt 2…\"". |
| The CHIP view's thinking lead blank dropped (separate arm). | `one_blank_line_above_thinking_badge` | "chip view: one blank line above the badge, got \" ❯ child prompt…\"" — both arms carry their own observer, so neither can regress behind the other. |
| `mark::HEADER` regressed to the 16-col block map. | `header_mark_uses_halfblock_glyphs` | Verbatim row pin fails: `left: " ██  ██  ██ ▀██▄"` vs the rebuilt `"   ▄▄  ██   ▄▄  ▀▀▀▀▀██▄"`. |
| `LIGHT.dim` washed to `#bdbdbd` (~1.9:1 on pearl). | `theme_tests::every_theme_clears_wcag_contrast_floors` | "Light: dim is 1.85:1 on the ground, floor 3:1" — the floors LAW names theme and token (the v0.0.62 lesson: distinctness alone let a washed token through). |
| `DARK.sel_bg` back to the gold blend (`gold.over(bg, 100)` — the warm wash returning to the chrome). | `theme_tests::dark_blends_match_hand_computed_goldens` | `left: Rgb(35, 32, 20)` vs the pinned neutral `Rgb(39, 39, 39)` — this pin is also what keeps the probe ladder's `48;2;39;39;39` byte constant honest. |
| `session::classify`'s `chip_or_session` routes chip-scoped payloads to the Session. | `child_view_renders_user_messages` | `row containing "audit the toolset and report gaps" not rendered` — the chip view loses its FIRST user message while the session would swallow it. |

## Structural notes (not mutations)

- **Both palettes, one identity.** The dark redesign changes GROUNDS and
  blends, never the accents: gold/ember hexes are byte-identical, so of the
  four raw SGR constants in `scripts/tui-probes/`, only the two GROUND
  constants moved (`pty-probe-cursor.py` SEL_BG, `pty-probe-sub.py` sticky
  barBg); GOLD_BG and the anim gold/ember fg rows kept their bytes and their
  ladder rows stayed green unedited — a live cross-check that the accents
  really did survive.
- **The mark rebuild is a derivation, not a redraw.** The banner's strokes
  are all 2-px pairs, so the half-resolution map is exact (baseline → `▀`
  rule, descenders → `▄`/`█` bumps, the `ـد` upright keeps a full pair →
  solid `█`); only inter-letter air was tightened for 28 → 24. The
  `header_fits` dignity gate moved with `HEADER_COLS` (art tier now needs
  ≥ 62 session cols / ≥ 52 launcher cols; below that the one-line text mark
  returns, never a clipped map).
- **Breathing rows ride the stream, not the chrome.** The trailing blank is
  a transcript `Line`, so it bottom-anchors with the tail, scrolls with
  history, and costs nothing when the transcript is short (top-anchored).
  The retired `band_pad` was chrome: it padded the band even when the
  transcript was empty and made the band read two rows tall at rest.
- **Honest pin flips, each with its rationale in-comment:**
  `tui3_visual_hover_tests::composer_band_is_the_exact_designed_blend_in_every_theme`
  (light/dark inputBg blends re-grounded; pad row → closing rule),
  `tui4_owner_wave_tests::the_composer_band_fills_its_whole_region_and_closes_with_a_rule`
  (pad sweep → rule-directly-beneath),
  `tui6_softwrap_tests::header_mark_reads_as_letterforms_at_24_cols`
  (renamed from the 16-col anatomy pin; new-row anatomy),
  `hit_alignment_tests::launcher_composer_is_bottom_anchored_with_the_gold_rule`
  (the wider mark ellipsizes the launcher info line ~8 cells earlier at 118;
  the full dirline content is pinned at 130 and the ellipsized dir at 118).
- **Item 6 fabricates nothing.** The chip view already routed chip-scoped
  `UserMessage`s through `classify` → `chip.transcript.apply`; the wave's
  deliverable is the END-TO-END pin (manifest spawn → first chip-scoped user
  message → sigiled row in the chip view, absent from the session view) so
  the daemon's spawn-prompt ui-flag fix (parallel lane) lands on a surface
  that provably renders whatever arrives.
