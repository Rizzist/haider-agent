# W-G — live throughput indicator: mutation ledger

Four EXECUTED kills, each on COMMITTED code (commit-before-mutation
satisfied by the three impl commits `01a953f` / `8b55a23` / `79751dc`).
Protocol per kill: confirm the single anchor with `python3` asserting
`src.count(old) == 1`, apply the mutation, run the ONE named law in
isolation (output reads `running 1 test`), record the observed RUNTIME
failure, `git checkout --` the file, re-run the law green. All ran under
`CARGO_INCREMENTAL=0`.

Covers the four brief-required kills: the p95 math (WG1), the sparkline
mapping (WG2), the idle no-op (WG3), the fallback honesty (WG5).

---

## Kill 1 — WG1 (p95 is nearest-rank, correct on a known distribution)

- File: `crates/haider-tui/src/throughput.rs` — `percentile`.
- Anchor (`count == 1`):
  `let rank = (usize::from(p) * n).div_ceil(100).clamp(1, n);`
- Mutation: `.div_ceil(100)` → `.div_ceil(50)` — halves the rank divisor, so
  the nearest-rank index overshoots and p95 clamps to the max.
- Test: `throughput::tests::wg1_percentile_is_nearest_rank_and_correct_on_a_known_distribution`
  (`running 1 test`).
- Observed failure (p95 of `1..=100` is no longer 95):
  ```
  assertion `left == right` failed
    left: Some(100)
   right: Some(95)
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 2 — WG2 (the sparkline ramp maps magnitude to the right glyph)

- File: `crates/haider-tui/src/throughput.rs` — `spark`.
- Anchor (`count == 1`): `let idx = ((offset * top) + span / 2) / span;`
- Mutation: replace with `let idx = 0u64;` — pins every column to the floor
  glyph, so a rising series stops ramping.
- Test: `throughput::tests::wg2_sparkline_ramps_monotonically_and_never_panics`
  (`running 1 test`).
- Observed failure (the last column of `[1..=8]` should be the top glyph):
  ```
  assertion `left == right` failed
    left: Some('▁')
   right: Some('█')
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 3 — WG3 (off-stream the row is hidden — zero idle cost)

- File: `crates/haider-tui/src/app.rs` — `throughput_readout`, the streaming
  gate.
- Anchor (`count == 1`, multi-line):
  ```
  if !self.projection.is_streaming() {
      return None;
  }
  ```
- Mutation: `if !self.projection.is_streaming() {` →
  `if false && !self.projection.is_streaming() {` — the gate never returns
  `None`, so a NON-streaming turn still surfaces the stale tracker readout.
- Test: `wg_throughput_tests::wg3_off_stream_hides_the_row_even_with_a_populated_tracker`
  (`running 1 test`).
- Observed failure (a `Done` turn leaks a readout the render row would draw):
  ```
  panicked at crates/haider-tui/tests/wg_throughput_tests.rs:83:5:
  off-stream: the gate hides the row regardless of tracker contents
  ```
- Reverted → `test result: ok. 1 passed`.

## Kill 4 — WG5 (the fallback is honest — approx never claims exact)

- File: `crates/haider-tui/src/throughput.rs` — `ThroughputTracker::readout`,
  the approx flag.
- Anchor (`count == 1`): `approx: !self.exact_seen,`
- Mutation: `approx: !self.exact_seen,` → `approx: false,` — a text-derived
  (no-usage) readout stops marking itself approximate, so an estimate would
  render as a measured `126 tps` with no `~`.
- Test: `throughput::tests::wg5_approx_source_is_marked_and_never_fabricates_exact`
  (`running 1 test`).
- Observed failure:
  ```
  panicked at crates/haider-tui/src/throughput.rs:432:9:
  no exact usage → the ~ marker
  ```
- Reverted → `test result: ok. 1 passed`.

---

After all four reverts: `git status` clean; the pure suite
`cargo test -p haider-tui --lib throughput` → `13 passed`; the render suite
`cargo test -p haider-tui --test wg_throughput_tests` → `7 passed`;
`cargo fmt --all -- --check` exit 0.

## Review of record (coordinator, Fable)

Re-executed the WG1 p95 kill myself (the stat core): `div_ceil(100)` →
`div_ceil(50)` in `percentile()` → `wg1_percentile_is_nearest_rank_and_
correct_on_a_known_distribution` FAILED with `left: Some(100)` vs
`right: Some(95)` — the law genuinely observes nearest-rank correctness.
Reverted; 13/13 throughput unit laws green. The lane's other 3 kills
(sparkline mapping, idle no-op, fallback honesty) spot-checked against the
notes; consistent. Honest ledger note: the 13 pure-tracker laws are inline
in src/throughput.rs (xtask counts only tests/ + *_tests.rs, so +7 counted
from wg_throughput_tests.rs; the 13 inline are disclosed, same class as the
G3 inline-module blindspot — a future move to throughput_tests.rs would
surface them). Campaign ACCEPTED.
