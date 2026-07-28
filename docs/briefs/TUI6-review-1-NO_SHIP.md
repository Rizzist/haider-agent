# TUI6 review round 1 — NO_SHIP

Reviewer: gpt-5.6 (codex), frozen 1f8a8a1, scope 1324633..1f8a8a1 (TUI6a-d). The independent pass found what the internal SHIP+CLEAN verification missed — the TUI5 precedent repeats (internal clean, independent finds the geometry class).

Required fixes (TUI6.1):
1. **P1 — stale wrap geometry across resize** (composer.rs:72, runtime.rs:274, render.rs:2671): Resize only dirties the model; a queued event can win select! before redraw updates wrap_budget + hit map. Nav/clicks then consume the PREVIOUS layout (repro: 20→10 col resize, Down lands byte 19 vs correct 9; stale click maps byte 4 vs 14). Resize does not bump the text revision so the stale-hit guard accepts. Fix so stale-layout consumption is impossible (geometry epoch gating hits+nav, or reflow-before-input) — reviewer's exact repros become pins.
2. **P2 — two-rule law at tight heights, five surfaces** (render.rs:324/:824/:1585/:1905): launcher 90×4, session+chip 90×11, session menu 90×10, subagent 90×11 + question 90×14, aura 90×10 all keep optional content while the closing rule sheds. Law: the lower rule is RESERVED whenever top rule + sacred input + bottom rule physically fit — ahead of optional panels, pad, breathing rows. Height-sweep tests per surface, not per-height point fixes.
3. **P2 — zero-width leading cluster** (render.rs:2749, composer.rs:798): standalone "\u{301}" cluster under the cursor paints no caret; click charges it an invented cell (.max(1)) shifting later byte mapping. Render and click must agree (give the zero-width cluster a real cell under the cursor, or normalize its width consistently in BOTH).
4. **P3 — chip-view band PTY evidence** rides fix 2 (pty-probe-sub gains the subagent composer/question band checks).

Otherwise: grapheme-wrap law HOLDS (203 adversarial checks), no-ellipsis HOLDS, dignity gate HOLDS (threshold drop = exactly the raster reduction), directed re-scopes all APPROVED/STRENGTHENED, mutation audit 3/3 killed+restored, gates green (live ladder rows environmental in reviewer sandbox; 16/16 here).

## 1. Findings

1. **P1 — resize can apply navigation or clicks using the previous frame’s wrap geometry.** [`composer.rs:72`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:72), [`runtime.rs:274`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:274), [`render.rs:2671`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2671). `Resize` only dirties the model; another queued event can win `select!` before redraw updates `wrap_budget` and the hit map. Reproduction: render 20 columns/budget 15 with cursor byte 4, dispatch resize to 10 columns, then `Down` before redraw. The cursor lands at byte 19; current-width geometry lands at byte 9. A stale click at `(8,26)` similarly maps to byte 4 versus byte 14 after reflow. Resize does not bump the text revision, so the stale-hit guard accepts it. This is the TUI5 stale-geometry class and blocks release.

2. **P2 — the two-rule band law still fails at tight but nondegenerate heights.** [`render.rs:324`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:324), [`render.rs:824`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:824), [`render.rs:1585`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1585), [`render.rs:1905`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:1905). The height-14 session claim-order regression is fixed, but neighboring and sibling layouts still let non-sacred content consume the closing rule:

   - Launcher 90×4: optional content, top rule, composer and status survive; bottom rule does not.
   - Session with chip 90×11: subtree/home rows survive without a bottom rule.
   - Session menu 90×10: a blank gap survives where the closing rule could fit.
   - Subagent composer 90×11 and question card 90×14: subtree survives without closure.
   - Aura 90×10: orb/transcript survive while the bottom rule sheds.

   The lower rule must be reserved whenever top rule + sacred input + bottom rule physically fit, ahead of optional panels, pad and breathing rows.

3. **P2 — a leading zero-width grapheme can hide the caret and disagrees with click geometry.** [`render.rs:2749`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2749), [`composer.rs:798`](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:798). With draft `"\u{301}a"` and cursor byte 0, the cursor-styled combining cluster occupies no terminal cell, so no caret is painted. Click mapping instead charges it one invented cell through `.max(1)`, shifting the visible `a`’s byte mapping. Ordinary decomposed `e\u{301}`, flags, skin tones, ZWJ families and CJK passed; the standalone cluster remains reachable through paste.

4. **P3 — direct subagent/chip-view PTY evidence is absent.** [`pty-probe-sub.py:101`](/Users/rizzist/haider-run/haider-tui2/scripts/tui-probes/pty-probe-sub.py:101). The probe validates Aura and final session bands, not the subagent composer/question band. TestBackend coverage is sufficient to establish generous-height rendering and would not block by itself, but it missed the tight-height failures above; the band fix should add direct chip-view PTY coverage.

## 2. Law table

| Law | Ruling | Evidence |
|---|---|---|
| Grapheme-wrap | **HOLDS** | `wrap_rows` walks extended graphemes. A char-boundary mutation failed on a mid-cluster row start. 203 adversarial boundary/navigation checks passed. |
| No-ellipsis | **HOLDS** | No horizontal draft window remains; draft rows never emit `…`. Remaining ellipses are unrelated chrome/palette strings. |
| Caret-visible | **VIOLATED** | Normal five-row vertical windows retain the caret, but a standalone zero-width cluster hides it; at the physically undersized width-6/CJK case the end caret is clipped. |
| Reduction consistency: render = click = nav | **VIOLATED** | Exact wrap-column clicks and drag agree in a stable frame, but resize-before-redraw makes navigation/clicks consume the previous layout. Zero-width click geometry also differs from rendering. |
| Model stores no wrap state | **VIOLATED** | No wrap points are stored, but `Composer::wrap_budget: Cell<usize>` stores layout geometry and directly produced the stale-frame reproduction. |
| Two-rule-per-surface | **VIOLATED** | All eight enumerated forms pass generously sized renders, but launcher, session/menu, subagent/question and Aura lose the lower rule while optional rows remain at tight heights. |
| Dignity gate | **HOLDS** | Header raster is exactly 16×4; banner remains 28×8. The threshold 62→54 is exactly the eight-column raster reduction while the 38-column reserve remains unchanged. At 54 columns mark/product/version fit and directory text clips last; at 53 the text fallback is used. |
| Zero-idle-wakeup untouched | **HOLDS** | The scoped change does not alter the guarded clean-model frame tick or animation scheduling. |

Stable-frame geometry controls passed: exact wrap clicks mapped row-0 last content to byte 9, the reserved boundary and row-1 first cell to byte 10, and drag selected `[9,10]`. Home/End remained logical-line edges. No interior-byte cursor state was constructible through navigation, click, drag or mutation. Budget-zero fallback, cap growth, sticky columns and shift-extension passed. The 1–16 height sweep retained the caret on session, subagent and Aura.

## 3. Directed-change rulings

1. `overlong_composer_line_keeps_the_cursor_visible`: **APPROVED/STRENGTHENED**. Replaces the obsolete tail-window ellipsis assertion with wrapped-head adjacency, no ellipsis and a styled tail caret.

2. `overlong_line_windows_around_a_mid_text_caret` → `overlong_line_wraps_around_a_mid_text_caret`: **APPROVED/STRENGTHENED**. Requires multiple visual rows, bans ellipses across them and preserves Home/caret/edit behavior.

3. `tail_window_never_splits_a_grapheme` → `wrap_rows_never_split_a_grapheme`: **APPROVED/STRENGTHENED**. Transfers the original no-mid-cluster law to every wrap point, checks row partitioning, combining marks and ZWJ families across multiple budgets.

4. `line_up` documentation/behavior re-scope: **APPROVED**. Correctly removes the no-wrap reduction, identifies visual-row navigation and preserves budget-zero logical-line behavior.

Test integrity is otherwise clean: counts progress monotonically `812 → 821 → 829 → 831 → 832`; no tests were deleted. Test history showed no other weakening.

Deferred adjudications:

- Missing chip-view PTY evidence: nonblocking in isolation, but should accompany the required band correction because existing TestBackend tests are generous-height and the pressure test’s “adjacent means full shed” skip masks non-sacred survivors.
- One-row window showing only the hidden-above `⋮`: accepted as an honest degenerate; the caret/content row remains visible.
- Over-wide grapheme at terminal width 6: accepted as a documented physically undersized degenerate, not a separate blocker.
- TUI6d claim-order cleanup: valid for its exact height-14 session regression, but incomplete across neighboring heights and sibling ledgers.

## 4. Mutation-check audit

Independently executed three mutations using scripted patches and restored each without checkout:

1. Replaced grapheme wrapping with character-boundary wrapping. `wrap_rows_never_split_a_grapheme` failed with row start 54 inside a cluster; restoration passed.
2. Disabled the launcher’s lower-rule render block. `launcher_band_carries_both_rules` failed because the closing rule disappeared; restoration passed.
3. Forced wrapped click windows to start at byte 0. `click_maps_through_the_wrapped_row_window` failed on the second row’s byte range; restoration passed.

Full gates:

- Release build: passed.
- `cargo test --workspace`: passed; no `FAILED` or `could not compile`.
- Clippy, all workspace targets with warnings denied: passed.
- Formatting check: passed.
- `xtask test-count`: `832/832`.
- PTY ladder: all 14 demo rows passed. Both live rows were environmental: direct daemon startup failed binding its Unix socket with `Operation not permitted (os error 1)`, matching the sandbox exception described in the brief.
- Merge-tree against `d9d66b4`: no conflicts and no overlapping changed paths; even the anticipated `test-baseline.txt`/`Cargo.lock` conflict surface did not materialize.
- Final HEAD remains `1f8a8a18f9ff7db12ec7e7a4a1f98ce0c34389fd`; porcelain, tracked diff, cached diff and stash are empty.

VERDICT: NO_SHIP
