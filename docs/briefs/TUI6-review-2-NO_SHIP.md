# TUI6 review round 2 — NO_SHIP (converging: every r1 pin passes; the adjacent seams remain)

Reviewer: gpt-5.6 (codex), frozen a365215, scope 33a3a7d..a365215 (TUI6.1 + TUI6.1b).

Required fixes (TUI6.2):
1. **P1 — stale `sticky_col`** (composer.rs:464/:499): the cached wrap-row-relative column survives budget changes AND draft swaps (repro: Down lands 24 vs correct 22 after resize). Invalidate/recompute whenever the effective budget changes; shift-selection shares the path.
2. **P1 — empty render never publishes its width** (render.rs:2699 early return before set_wrap_budget:2745): a fresh empty composer keeps budget 0; type + queued nav before redraw walks logical lines (cursor 4 vs 17). Publish the budget before the empty-branch return.
3. **P1 — Aura→Session chip-close draft leak** (app.rs:2617): close_chip_state assigns Screen::Session directly, bypassing stash/restore — an Aura draft leaks onto the session surface and can be SUBMITTED there. Pre-existing, but disproves the single-seam claim: make direct Screen assignment unrepresentable (one switch_surface authority owning stash/restore).
4. **P2 — the question card lost its title** (the r1 band fix funded the rule by shedding the title at 90×12; queueing OVERRULED — options without their question is a dignity regression): floor_input funds title+options with session parity; de-blind sweep_two_rules (tui6_softwrap_tests.rs:1050 continues when the needle is absent — a title-less frame passes unnoticed).
5. **P2 — login-card close skips restore** (stash at app.rs:2452; Esc/Ctrl-C only clear the card at :2407): the parked composer AND ITS HISTORY are stranded and can be overwritten. Pair it (restore on close) — the earlier safe-adjudication covered text only, not history.
6. **P3 — the single band law is debug-only on launcher/aura** (render.rs:2415 claim): their release ladders are duplicated arithmetic; the band_rule_reserve tie compiles out. Route the release ladders through the function (runtime authority, not advisory).
7. **P3 — scratch selection render epoch bump** (runtime.rs:2031) can transiently drop a queued click (fails CLOSED — safe): document or ledger.

Also fold the parked TUI6.2 queue (scratchpad tui62-queue.md): promote the verifier's s1-s6 seam attacks into shipped pins; the login empty-draft pin rides fix 5.

Validated by r2: all r1 repro pins pass; width policy CLOSED (zero-width after wide + on-boundary probes agree everywhere); epoch sound (u64, monotonic, no ABA); band output law holds at every swept height; mutation audit incl. all-five-at-once confirmed (with the caveat that launcher/aura died via debug_assert only → fix 6); merge-tree clean vs d9d66b4.

## Closure

| Round-1 item | Ruling | Re-executed result |
|---|---|---|
| 20→10 resize, queued Down: 19 vs 9 | **PARTIAL** | Exact pin now lands byte **9**, and removing reflow returns **19**. The class remains open through stale `sticky_col` and empty-render budget paths. |
| Stale resize click/drag | **CLOSED** | Pre-resize hit is rejected without moving/arming drag; current-frame hit maps correctly. Epoch gates cover press and drag. |
| Tight-height two-rule law | **CLOSED** | All exact frames pass: launcher 90×4, session+chip 90×11, session menu 90×10, subagent 90×11/question 90×14, Aura 90×10. The “one runtime law” implementation claim is separately false. |
| `"\u{301}a"` caret and click | **CLOSED** | Space-base synthesis paints the caret; render, click, wrap, and navigation agree. |
| Self-found TUI6.1b budget across draft swap | **PARTIAL** | Exact restore pin lands byte **14**, not stale **5**. However, `sticky_col` travels with the parked draft, and some surface changes bypass restoration entirely. |

The chip-view PTY evidence is closed: both composer and question-card checks passed in the tall `pty-probe-sub.py` row.

## New-mechanism attack

### (a) Resize, epoch, and swap seams

The three intended mechanisms work for their exact cases:

- Resize installs the new budget before queued input at [runtime.rs:429](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:429).
- Hits are stamped and gated at press and drag.
- Normal draft restoration carries the current budget at [app.rs:1508](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1508) and [app.rs:1535](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:1535).

Two uncovered geometry consumers remain:

1. **P1 — stale sticky navigation column.** `sticky_col` caches a wrap-row-relative column at [composer.rs:464](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:464) and [composer.rs:499](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:499), but changing `wrap_budget` does not clear it.

   Executed repro: budget 13, caret 4, Down → 17/cache column 4; resize to budget 5; Down → **24**. Recomputing the current row column, now 2, lands **22**. Shift-selection shares this path. Parking/restoring the draft preserves the same stale column and also lands 24.

2. **P1 — empty render never publishes its width.** The empty-composer branch returns at [render.rs:2699](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2699), before `set_wrap_budget` at [render.rs:2745](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2745).

   Executed repro: fresh empty Launcher rendered at width 18 retains budget 0; type 30 characters and queue Home, Right×4, Down before redraw → cursor remains **4**. The painted frame’s correct budget 13 moves to **17**.

Epoch analysis:

- Model/rendering are single-loop owned; the input thread only sends events. There is no `Cell` data race.
- Two renders increment monotonically, so an older hit cannot acquire the current epoch.
- The epoch is `u64`; ABA requires \(2^{64}\) increments—about 19.5 billion years at 30 renders/second.
- Scratch selection rendering also increments the epoch at [runtime.rs:2031](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/runtime.rs:2031). A queued click can therefore be dropped until the pending redraw, but it fails closed rather than accepting stale geometry.

Hover carries no composer-byte geometry. Sticky transcript jumps are value-carrying and clamped, not another composer-wrap seam.

### (b) `band_rule_reserve`

| Ledger | Runtime relationship to the function | Attack result |
|---|---|---|
| Session | Calls the function, then takes budget or the gap before optional panels | Lower rule remains funded; no later claimant removed it. |
| Subagent | Calls the function after the subtree/gap ladder | Rule remains funded, but a 90×12 four-option card preserves a blank gap while shedding the question title. |
| Launcher | Duplicated ladder; function appears only in `debug_assert_eq!` | Current arithmetic agrees, but the tie disappears in release builds. |
| Aura | Duplicated ladder; function appears only in `debug_assert_eq!` | Same: advisory debug tie, not runtime authority. |

Thus the lower-rule output law passes its exact pins, but the “stated once and applied by every surface” claim at [render.rs:2415](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2415) is incorrect. Launcher and Aura do not route release behavior through the function. Zeroing it leaves their release ladders unchanged; the debug mutation kills them only through assertions.

No current height produced a funded lower rule that was later removed. The subagent 90×12 counterexample is instead a priority defect: `floor_input = options.len()` funds four options, not title plus options, while the optional gap survives.

### (c) Width-policy unification

**CLOSED.**

`wrap_rows`, sticky-column calculation, `seek_col`, and `byte_at_col` all use `cluster_cells`/`cluster_cols` at [composer.rs:707](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/composer.rs:707). The renderer’s raw `.width() == 0` at [render.rs:2848](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/render.rs:2848) is only the space-base synthesis predicate. No stray composer numeric geometry uses `UnicodeWidthStr`.

Executed boundary probe with `"界\u{200b}a"`:

- Budget 2 → rows `"界"` (2 cells), `"\u{200b}a"` (2 cells).
- Budget 3 → rows `"界\u{200b}"` (3 cells), `"a"`.
- Rendering emits the synthesized space for U+200B and click offsets agree.

This covers a zero-width cluster after a wide glyph and directly on a wrap boundary.

### (d) Draft lifecycle

Normal session/Launcher/Aura transitions, `/reset`, and production `fresh_session` callers pair stash/restore. Session and Subagent intentionally share a draft key; no session-deletion path was found.

The seam is not unique:

- **P1 — asynchronous chip close bypass.** `close_chip_state` directly assigns `Screen::Session` at [app.rs:2617](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2617). Executed repro: Aura with `"aura draft"` → background chip close → Session still containing `"aura draft"`. This can submit Aura text to the session surface.
- **P2 — login close lacks restoration.** `/login … api` stashes at [app.rs:2452](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2452), but Esc/Ctrl-C merely clear the card at [app.rs:2407](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/src/app.rs:2407). The parked composer/history remains inaccessible and can later be overwritten.

Both bypasses predate these two commits, but directly disprove the claimed single restoration seam.

## Carried P3 adjudication

**Queueing overruled; release-blocking P2 regression.**

At 90×12 with four choices:

- `33a3a7d`: question title on row 5, options on rows 6–9, no lower rule.
- `a365215`: options on rows 5–8, lower rule on row 9, blank optional gap on row 10, **no question title**.

TUI6.1 therefore traded away the question’s semantics to obtain the rule. Options without their question violate dignity and are regressed, not improved.

The height sweep is needle-blind: when its title needle is absent it `continue`s at [tui6_softwrap_tests.rs:1050](/Users/rizzist/haider-run/haider-tui2/crates/haider-tui/tests/tui6_softwrap_tests.rs:1050), so the title-less frame passes unnoticed.

## Mutation audit

| Executed mutation | Result |
|---|---|
| `band_rule_reserve` forced to zero | All five `reserved_rule_*` tests failed together; restored, all five passed. Launcher/Aura deaths were debug-assert deaths, exposing the non-load-bearing release tie. |
| Removed reflow-before-input | Exact resize test failed **19 vs 9**; restored and passed. |
| Removed budget carry in `restore_draft` | Swap test failed **5 vs 14**; restored and passed. |
| Removed zero-width space base | Leading-combining caret lost its gold cell; restored and passed. |

All mutations were scripted and individually restored. Final tracked, cached, porcelain, diff-check, and stash state is empty.

## Law table

| Law | Ruling |
|---|---|
| Grapheme-wrap | **HOLDS** |
| No ellipsis | **HOLDS** |
| Caret visible | **HOLDS** for supported geometry and zero-width clusters; documented physically undersized degenerates remain accepted. |
| Render = click = navigation, including resize and swap | **VIOLATED** by stale `sticky_col`, empty-render budget, and the Aura close transition. |
| Model stores no wrap state | **VIOLATED**: `wrap_budget` and row-relative `sticky_col` are geometry state, and both have produced stale behavior. |
| Two-rule reserved | **HOLDS** at the exact/swept output pins; **not unified by one release runtime authority**. |
| Dignity | **VIOLATED** by the title-less four-option question frame. |
| Zero idle wakeup | **HOLDS**; guarded clean-model frame scheduling is unchanged. |

## Test integrity, gate, and merge readiness

- Baseline is monotonic: **832 → 841 → 842**.
- `xtask test-count`: **842 tests / baseline 842**.
- Directed re-scopes are sound:
  - Queue-ledger height moved to 90×11 with an added 90×10 shed assertion.
  - Aura orb moved behind the tall gate because it now legitimately sheds at 90×10.
  - Screen-dump label matches the new shed order.
- Release workspace build: passed.
- Clippy, all targets, warnings denied: passed.
- Formatting: passed.
- Full `cargo test --workspace --no-fail-fast` reached every target with no `could not compile`. Eight UDS-dependent targets/88 tests failed solely because this sandbox rejects Unix-socket creation with `PermissionDenied`; all non-UDS targets passed.
- PTY ladder: **14/14 demo rows passed**. Live rows are not independently runnable under the same UDS denial; the supplied orchestrator result is 16/16.
- Synthetic merge-tree against `d9d66b4`: clean, no conflicts.
- Final HEAD remains `a36521519a1876af1429b7bef254f0effac0f39d`, byte-clean.

## New findings by tier

- **P0:** none found.
- **P1:** stale `sticky_col` across resize/swap; empty-render budget omission; asynchronous Aura→Session chip-close draft leak.
- **P2:** newly title-less question card; login-card close skips draft restoration.
- **P3:** launcher/Aura single-law ties are debug-only; scratch selection rendering can transiently invalidate the live hit map and drop a click safely.

Merge mechanics are ready, but the resize class, surface-swap discipline, and question-card dignity gate are not closed.

VERDICT: NO_SHIP
