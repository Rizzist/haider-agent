# W-E — Thinking-verb shimmer: mutation ledger

Four EXECUTED kills, each on committed code (commit-before-mutation
satisfied by `0bbfabd`). Protocol per kill: single-anchor confirmed with
`python3` asserting `src.count(old) == 1`, apply the mutation, run the ONE
named law (filtered so the output reads `running 1 test`), record the
observed runtime failure, `git checkout --` the file, re-run the law green.

Covers the four brief-required laws: sweep advance (LE2), scope restriction
(LE3), idle no-op (LE4), degradation path (LE6). All ran under
`CARGO_INCREMENTAL=0`.

---

## Kill 1 — LE2 (the sweep advances)

- File: `crates/haider-tui/src/style.rs` — `shimmer_centre`.
- Anchor (`count == 1`): `(pos < len).then_some(pos)`
- Mutation: `(pos < len).then_some(pos)` → `(pos < len).then_some(0)`
  — pins the crest to glyph 0 (a static "always bright at 0").
- Test: `le2_the_sweep_travels_and_wraps` (`running 1 test`).
- Observed failure:
  ```
  assertion `left == right` failed: one glyph per tick, then a short tail rest
    left:  [Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), Some(0), None, None, None]
    right: [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7), None, None, None]
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 2 — LE3 (only the verb glyphs shimmer)

- File: `crates/haider-tui/src/render.rs` — `thinking_line`, the `…` span.
- Anchor (`count == 1`):
  `spans.push(Span::styled("…", theme.gold_style()));`
- Mutation: style the `…` with
  `theme.shimmer_ink(phase, len - 1, len, truecolor)` — folds the trailing
  ellipsis into the wave so it stops being static base ink.
- Test: `le3_only_the_verb_glyphs_shimmer` (`running 1 test`).
- Observed failure:
  ```
  assertion `left == right` failed: the `…` never shimmers (phase 6)
    left:  Rgb(230, 212, 155)   # the mid shoulder leaked onto the ellipsis
    right: Rgb(217, 181, 68)    # base gold
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 3 — LE4 (idle costs nothing)

- File: `crates/haider-tui/src/render.rs` — the session thinking gate.
- Anchor (`count == 1`, multi-line block):
  `if model.projection.is_thinking() {` + its `// S2 item 5…` comment.
- Mutation: `if model.projection.is_thinking() {` →
  `if true || model.projection.is_thinking() {` — renders the animated tail
  even when no run is in flight.
- Test: `le4_idle_status_line_is_byte_identical_across_ticks`
  (`running 1 test`).
- Observed failure (idle frames diverge across ticks — the tail appears and
  the dot breathes):
  ```
  assertion `left == right` failed: idle frames are phase-invariant — the shimmer costs nothing at rest
    left:  [... " ● thinking…" ...]   # phase 0
    right: [... " ◌ thinking…" ...]   # phase 137
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 4 — LE6 (non-truecolor degrades to two-tone)

- File: `crates/haider-tui/src/style.rs` — `shimmer_ink`.
- Anchor (`count == 1`, multi-line):
  ```
  let ink = if truecolor || level != 1 {
      inks[level]
  } else {
      inks[0]
  };
  ```
- Mutation: replace with `let ink = inks[level]; let _ = truecolor;` — the
  degraded path stops collapsing the mid shoulder, so it emits the
  intermediate truecolor code a 16-color terminal must not see.
- Test: `le6_non_truecolor_degrades_to_two_tone_without_the_mid_code`
  (`running 1 test`).
- Observed failure:
  ```
  panicked at tests/we_thinking_shimmer_tests.rs:351:13:
  degraded mode emits no intermediate (mid) truecolor code
  ```
- Reverted → `test result: ok. 1 passed`.

---

After all four reverts: `cargo test -p haider-tui --test
we_thinking_shimmer_tests` → `7 passed`; `cargo fmt --all -- --check`
clean; working tree clean.
