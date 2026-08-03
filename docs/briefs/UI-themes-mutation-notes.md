# UI-themes wave mutation notes

Every mutation below was EXECUTED against the suite on 2026-08-03 (apply →
observe the runtime failure → revert). "Expected RUNTIME failure" means the
named test fails by assertion; a compile-only failure is not the claimed
evidence. Observers live in `haider-tui/tests/ui_themes_tests.rs` unless
another file is named. The wave landed in three milestones on `ui-themes`:
m1 launcher-as-session layout, m2 the four designed palettes, m3 the theme
system (system default · `/theme` picker · profile-dir persistence).

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| `render_boot` drops the sanctum push (`if false && let Some(text)`) — the shahada leaves the boot splash. | `boot_splash_keeps_centered_shahada` | The "the shahada lives on the boot splash" assertion fails: the ceremony stack lost its centerpiece. |
| The launcher header band drops the device spans (` · ` + `identity.device`). | `launcher_renders_header_band_not_centered_art` | "device name in band" fails — the owner's header contract is wordmark · version · DEVICE. |
| `DARK.dim` regresses to the sim-era muddy `0x8a7b5e`. | `theme_tests::dark_refresh_keeps_the_identity_hexes` | The token equality fails (`left: Rgb { 138, 123, 94 }`). FINDING: this mutation SURVIVED `every_theme_clears_the_contrast_floors` — the m2 ground deepened (`0x14100a` → `0x120e08`), lifting the old ink to ~3.5:1, just over the 3.4 floor — so the identity-hex pin is the load-bearing observer for the dim refresh and is recorded here as such. |
| `LIGHT.gold` keeps the DARK palette's bright gold `0xd4af37` (the classic inverted-dark mistake the owner's spec forbids). | `every_theme_clears_the_contrast_floors` | "Light: gold contrast 1.95 under the 3.2 floor" — bright gold is illegible on paper. |
| A raw `Color::Red` const lands in `render.rs`. | `every_surface_uses_theme_slots` | "render.rs: raw `Color::` outside the style seam" — the mechanical sweep walks every `src/*.rs` and allows `Color::` only in style.rs, `Rgb::hex(` only in theme.rs, nothing in plain.rs. |
| `preview_theme_row` stops writing `self.theme` (highlight moves, nothing changes). | `theme_picker_lists_and_switches_instantly` | "row 2 previews light" fails with the theme still Dark — the owner's "applies instantly" law. |
| The picker's esc arm keeps the previewed theme instead of restoring `prior`. | `theme_picker_lists_and_switches_instantly` | "esc reverted the preview" fails with `left: Oasis` — esc must be a true revert. |
| `SettingsStore::load` drops the version gate. | `theme_persists_and_reloads` | The version-9 file loads as `Some(Fixed(Desert))` against the `None` expectation — a foreign future format must mean defaults, never a half-load. |
| `resolve_system_theme`'s undetectable arm falls back to `Light`. | `system_theme_follows_detection_fallback_dark` | `resolve_system_theme(None, None)` returns Light against the pinned Dark — the owner's fallback law. |
| `theme_from_colorfgbg` parses the FIRST field (the foreground) instead of the last. | `system_theme_follows_detection_fallback_dark` | `theme_from_colorfgbg("15;0")` returns `Some(Light)` (fg 15) against the pinned `Some(Dark)` (bg 0). |
| `hydrate` applies the demo file's theme again (the pre-wave behavior). | `tui4c_persistence_tests::guarded_singles_restore_theme_vfs_dir_and_voice` | "hydrate surfaces the legacy theme, never applies it" fails with `left: Desert` — the profile-dir settings file is the one theme-persistence authority. |
| The launcher shed ladder drops the three header-shed rungs (the band stops yielding). | `tui6_softwrap_tests::reserved_rule_sweeps_launcher` | The 90×4 floor pin fails: "optional content renders but the TOP rule is missing" — the band triple (top rule · composer · closing rule) must outlive the whole header. |

Structural notes (not mutations):

- The `/theme` picker is MODEL-LOCAL overlay state (`AppModel::theme_picker`),
  deliberately NOT a projection card: it can never ride a session
  checkout/stash, and a daemon card outranks it on both the key path
  (`menu_owns` in `handle_key`) and the render path
  (`theme_picker_showing`). It still renders through the shared
  `menu_block` anatomy so the owner menu law (numbered rows, ❯ arrow
  highlight, digit/click answers, windowed options under pressure) is
  inherited, not re-implemented.
- Previews write only the RESOLVED `model.theme`; the committed
  `theme_choice` moves on ⏎/digit/click alone — which is what the
  runtime's persistence watch keys on, so a preview can never touch the
  settings file. The resolved theme itself is never persisted: `system`
  re-reads the terminal on every boot by construction.
- Palette contrast floors were designed against a WCAG-luminance harness
  before the hexes were committed (all four palettes clear every floor on
  first principles, not by test-tuning); the test re-implements the
  luminance math as an independent oracle.
- The `every_surface_uses_theme_slots` sweep found the render path ALREADY
  clean (no `Color::` outside style.rs existed before this wave) — the law
  pins that state mechanically so it stays true.
