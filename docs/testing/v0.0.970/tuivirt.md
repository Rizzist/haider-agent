# Lane tuivirt — virtualized TUI transcript (v0.0.970)

Date: 2026-09-02  
Branch: `lane-970-tuivirt`  
Scope: `haider-tui`, its transcript pins, the client-footprint replay seam,
and ship-gate ledger row 17. No daemon, client-library, or RPC source changed.

## Result

Transcript frames now format and draw only the entries intersecting the
viewport plus two rows of overscan. Unmeasured history uses constant-sample
height estimates; measuring a visible entry records a sparse prefix
correction. Entry lookup binary-searches the global 64-bit row space, so opening
a session at its real tail, mid-history scrolling, sticky jumps, and reaching
row 0 do not require formatting all preceding entries.

The render cache is a distance-pruned `BTreeMap` capped at 96 entries. It owns
only the currently useful formatted lines and never clones raw
`TranscriptEntry` values. User prompt ordinals and active screen-control items
are maintained at ingest, removing the last full-history scans from the normal
frame path. Theme styles remain the shared `Copy` palette values. Width/theme
changes discard wrapping geometry and immediately measure only the new
viewport; append-only revisions preserve prior corrections.

Assistant entries above 64 KiB use a windowed formatter. A logical line above
4 KiB is visibly truncated with the cue
`extreme line truncated · /export expands raw text`; raw transcript text remains
authoritative for export/search. The original 1 MiB top/tail golden stays
byte-identical, and the window-retention test prevents a return to full-entry
formatted storage.

## Frame-shape evidence

The pre-lane 10k cold result is the owner-observed row-17 failure. Larger
pre-lane cold values are deliberately not extrapolated into measurements.
All after values below are from the always-on release shape test at 118×36;
model construction is outside every timed interval and there are no sleeps.

| Transcript rows | Before first frame | After first frame | After cached follow p95 | After cached middle p95 |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | ~255 ms (measured ship-gate failure) | 0.223 ms | 0.108 ms | 0.111 ms |
| 50,000 | not measured | 0.257 ms | 0.112 ms | 0.117 ms |
| 200,000 | not measured | 0.252 ms | 0.142 ms | 0.144 ms |

The worst observed frame is 0.257 ms, 99.2% below the 33 ms budget (99.2%
headroom, above registry #79's 25% requirement). Every cached p95 is below
0.15 ms and every first frame is below 0.26 ms. The gate's checked 20% rule
includes its pre-existing 1 ms absolute allowance for sub-millisecond timer
noise.

The same test asserts that the first frame contains the actual final row and
that setting the 64-bit scroll offset to its ceiling reaches `row 0` at every
size, including 200k rows.

## Retention evidence

The render-side figures come from the always-on, thread-scoped counting
allocator pin. “Model” is the single authoritative raw projection and is
reported separately; it is not part of the bounded render cache.

| Transcript rows | Before render retention | After model/raw | After render retention | Render ratio vs 1k |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 1,224 KiB | 562 KiB | 6 KiB | 1.00× |
| 50,000 | 66,384 KiB | 30,935 KiB | 6 KiB | 1.00× |

| Pathological entry | Raw size | Retained formatted window | Ceiling |
| --- | ---: | ---: | ---: |
| multiline assistant reply | ≥1 MiB | 28 KiB | 256 KiB |

The real release client was also sampled with
`client-footprint-budget.py --surface tui-demo-no-graphics` after a 60-second
settle. The paired replay samples held host load below four, but this sandbox
denied `vmmap` (exit 255), so they remain explicitly rejected diagnostics, not
accepted calibration. They still exercise the requested interactive client
surface and report Darwin RSS from `proc_pid_rusage`/`proc_pidinfo`.

| Replay visual rows | Resident RSS | Physical footprint | CPU at read | Threads | Evidence status |
| ---: | ---: | ---: | ---: | ---: | --- |
| 0 (ordinary demo) | 11,747,328 B | 4,309,376 B | 1,017,894 µs | 4 | rejected: `vmmap` denied; load 3.92/3.92 |
| 1,000 | 13,664,256 B | 5,980,568 B | 998,322 µs | 4 | rejected: `vmmap` denied; load 2.35/2.69 |
| 50,000 | 15,908,864 B | 8,225,176 B | 436,193 µs | 4 | rejected: `vmmap` denied; load 2.63/3.25 |

Resident RSS ratio is **1.164×** and Darwin physical-footprint ratio is
**1.375×**, both within the required 1.5×. An intermediate pre-index 50k run
consumed 25.7 seconds of CPU during the settle; after byte-range indexing and
allocation-clean replay construction the final 50k process consumed 0.44
seconds, confirming that animation frames no longer rescan all 50k raw lines.
The ordinary-demo physical footprint is below both the v0.0.969 provisional
6,110,043-byte TUI budget and the owner's 10.6 MB live-TUI baseline, so there is
no small-session regression signal. The deterministic render-retention pin
above remains the accepted memory gate.

## Behaviour and visual preservation

- All 13 pre-lane golden scenarios pass unchanged at 80×24, 118×36, and
  160×50; no fixture was regenerated. They cover markdown, fenced code,
  tool boxes, wide tables, CJK/emoji/combining marks, wrapping, menus,
  streaming, sticky history, and the 1 MiB reply.
- `extreme_logical_line_is_capped_with_raw_export_expander` adds the explicit
  pathological-line behaviour pin without changing an existing snapshot.
- All seven 10k scroll/cache pins pass, including warm/cold identity,
  follow-mode append semantics, jump-to-bottom, sticky-origin landing,
  resize invalidation, and edit/completion invalidation.
- Screen-control banner coverage remains green after replacing its frame scan
  with ingest metadata.

## CI ledger and registry walk

Ledger row 17 now runs
`tuivirt_shape_bench_tests` in `--release` at 10k/50k/200k. The old
`w3c3_render_bench_tests` cold-fill assertion no longer treats O(N) work as the
contract; its 10k first-frame check uses the 33 ms frame budget.

- #20/#21/#54: tests only increased; every Cargo command uses the mandated
  8 MiB test stack and the test census is regenerated at the end.
- #64: no daemon binary or daemon test is in this lane's scope.
- #79: worst measured release frame has 99.2% headroom over 33 ms.
- #94: no deadline was added to product code. The footprint harness's existing
  deadlines remain derived/documented; no fixed sleep appears in the shape
  test.
- #95: no connection or external-state wait was added.
- The supplied `LANE-COMMON.md`, `LANE-BRIEF-tuivirt.md`, `turnperf/`, and
  `turnperf2/` evidence remains unmodified and uncommitted.

## Verification commands

- `cargo test --release -p haider-tui --test tuivirt_golden_tests --test
  tuivirt_scroll_tests --test tuivirt_memory_tests --test
  tuivirt_shape_bench_tests --test w3c3_render_bench_tests --locked --
  --nocapture` — pass: 14 golden, 7 scroll, 3 memory, 2 shape, and 1 legacy
  render-benchmark test; zero ignored acceptance tests.
- `cargo test -p haider-tui --locked` — pass for the entire TUI crate and all
  integration targets.
- `cargo clippy -p haider-tui --all-targets --locked -- -D warnings` — pass.
- `cargo fmt --all -- --check` and `git diff --check` — pass.
- `cargo run -q -p xtask --locked -- test-count` — pass at 4,393 tests
  (baseline 4,393); the baseline had first been regenerated with
  `test-count --update`.
- `python3 -m py_compile scripts/perf/client-footprint-budget.py`, its
  `--self-test`, and all eight `test_client_footprint_budget.py` tests — pass.
- The repository has no root `run.sh`; the only match is the unrelated
  `scripts/qa-gate/run.sh`, which requires `--tier` and `--bin-dir` and has no
  `test` subcommand. The full scoped Cargo suite above is the applicable test
  entry point.
