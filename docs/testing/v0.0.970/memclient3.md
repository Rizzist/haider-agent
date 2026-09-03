# Lane memclient3 — client memory after tuivirt (v0.0.970)

Date: 2026-09-03
Branch: `lane-970-memclient3`
Scope: TUI-client terminal graphics, TUI prompt recall, and the existing
client-footprint measurement seam. No daemon, OAuth, RPC, tpsfix, or tuivirt
projection/cache source changed.

## Verdict

The client changes are implemented and the correctness suites are green. The
landed tuivirt baseline is already far below the older 10.6 MB owner datum. A
valid paired idle diagnostic measured 5,374,360 -> 4,506,008 bytes, a further
868,352-byte (16.2%) reduction after first image use. The fixed-protocol
implementation releases the decoded 450×199 RGBA source rather than retaining a
resizeable image for the life of the TUI, and optional prompt recall is now
bounded independently of the authoritative transcript.

The lane is nevertheless **NO_SHIP**. This sandbox denies `vmmap`, so local
physical-footprint runs are explicitly rejected diagnostics under the existing
calibration contract. Shared-host load repeatedly invalidated or prevented the
complete candidate N=5, strict warm ABBA, and exact 1.06 MiB peak hold-outs. The
target is met in every valid candidate observation and there is no latency or
peak regression signal, but incomplete/rejected diagnostics cannot satisfy the
acceptance contract or authorize changing the standing CI budget.

## Provenance and citation drift

- The supplied common brief cites an older base. Both `HEAD` and
  `origin/wave-970` were `6a374b148b4230ee1892d5aef20ab66c6d7008bf`
  when the lane was inspected and the baseline was frozen. No merge or rebase
  was performed.
- `tuivirt.md` is the landed authority: viewport-only formatting, sparse
  `BTreeMap` cache capped at 96 entries, and 6 KiB render-side retention at both
  1k and 50k rows. This lane did not replace or broaden that implementation.
- The held `lane-970-memclient2` branch predates the landed tuivirt source. Its
  dense transcript/cache edits were not imported. Only its Mach-timebase and
  fixed-turn measurement approach, plus the still-relevant prompt-recall and
  terminal-image observations, were re-audited against current source.
- The v0.0.969 owner datum remains 10.6 MB physical footprint, 12 threads, and a
  5,920 KiB `MALLOC_LARGE` region with 2,992 KiB dirty. Its attribution to the
  terminal wordmark was plausible, not proven. Current frozen-wave Sixel
  measurements are the correct direct before/after comparison for this lane.
- The supplied `LANE-COMMON.md`, `LANE-BRIEF-memclient3.md`, `turnperf/`, and
  `turnperf2/` inputs remain unmodified and uncommitted.

## Implemented changes

### Lazy, releasable terminal graphics

- Capability query ordering is unchanged: it still occurs after raw mode and
  before the input pump can consume the terminal response. A successful query
  now retains only a deferred `Picker`; it does not decode the embedded PNG.
- Empty or undersized image slots remain deferred. The first real 24×2 or 28×4
  draw decodes the PNG and constructs two fixed `Protocol` values matching the
  two shipped geometries. The fixed protocols retain their encoded terminal
  payload, but not the resizeable `DynamicImage` source.
- The decoded 450×199 RGBA buffer is exactly 358,200 bytes before image-library
  overhead. It and encoding scratch die at initialization completion; a
  one-shot Darwin allocator-pressure-relief request follows the first render.
- Decode/encode failure becomes a one-shot failed state, and render prepares
  before clearing the existing half-block mark. Halfblock-only terminals never
  create a graphics wordmark.
- Sixel, Kitty, and iTerm2 are all pinned by tests at 24×2 and 28×4. This avoids
  the v0.0.969 negative experiment that discarded the entire protocol and paid
  re-encoding cost on later frames.

### Post-tuivirt owned strings

The viewport projection already owns handles and its sparse render cache does
not clone `TranscriptEntry` values. The remaining material retained duplicate
was session-wide prompt recall: active and background journal paths copied every
user prompt into unbounded `String` deques in addition to the authoritative raw
projection.

Active, background, and demo paths now share one insertion helper capped at 128
entries and 1 MiB of actual retained `String::capacity()`. Oldest entries are
evicted until both laws hold. An individually oversized prompt is not copied
into optional recall, but remains byte-for-byte present in the authoritative
transcript. Durable sequence coordinates are retained for every admitted recall
entry, preserving backtrack/fork behavior.

Transient decode values, IDs, error-card text, and the authoritative transcript
were deliberately left alone; changing those would either be non-retained work
or would cross the viewport/session semantics already owned by tuivirt.

### Tokio runtime decision

No runtime code changed. Current source already selects `new_current_thread()`
for ephemeral headless commands and `worker_threads(2)` for the interactive TUI
and other full-runtime paths. The construction pins pass. The sandbox denies
`ps`, so the live resident-thread test reports its documented local skip rather
than fabricating an observed count; the TUI footprint binding itself reports
four process threads (main, two Tokio workers, terminal input).

### Measurement tooling

- Darwin task and rusage CPU counters are Mach ticks, not microseconds. The
  harness now obtains `mach_timebase_info`, retains raw ticks, and emits
  correctly converted ns/us values.
- `--tui-turns` accepts only 0 or exactly 20 and only on TUI surfaces. Each turn
  waits for THINKING followed by IDLE, then drains 250 ms (more than seven 33 ms
  frame intervals). The per-turn 90-second deadline is derived from the existing
  `2 × 45 s` headless terminal deadline.
- Load is checked immediately before both process CPU reads. `vmmap` collection,
  strict calibration rejection, the 60-second settle, the long-transcript
  replay seam, and the provisional CI budgets are unchanged.

## Frozen binaries

| artifact | SHA-256 | bytes |
| --- | --- | ---: |
| baseline `haider` | `db66dfe3e0df6f60fef6c5bfe40226a2ffb902b1f52a595597dd957739185103` | 34,781,152 |
| baseline `haiderd` | `ffe5cf2930231af1bfe5d5295c72e6be3e236d1626be3827155771f383d0eeee` | 52,556,160 |
| candidate `haider` | `c269169a0b9dd324d1dd7b72569fa175de894964af0765f803bf96b772a3552f` | 34,781,232 |
| candidate `haiderd` | `ffe5cf2930231af1bfe5d5295c72e6be3e236d1626be3827155771f383d0eeee` | 52,556,160 |

The candidate daemon is byte-identical to baseline and remains above registry
#64's 10 MiB release-binary floor.

## Settled Sixel footprint and fixed-turn CPU

The frozen baseline completed N=5 at 118×36 after the original 60-second settle
and then exactly 20 demo turns. Every sample observed alternate-screen and
graphics-query traffic and had four threads. `vmmap` exited 255 in every run, so
the table is rejected diagnostic evidence rather than accepted calibration.

| variant | physical footprint min / median / max | footprint MAD | 20-turn CPU min / median / max | CPU MAD | accepted |
| --- | ---: | ---: | ---: | ---: | --- |
| baseline | 5,865,880 / **6,160,792** / 6,291,864 B | 49,152 B | 1,985,093,834 / **2,336,329,583** / 2,546,556,917 ns | 210,227,334 ns | no — `vmmap` denied |
| candidate | 5,816,728 B (N=1 only) | — | 2,251,785,167 ns (N=1 only) | — | no — incomplete; `vmmap` denied |

The candidate sample is 344,064 bytes below the baseline median; its exact
20-turn CPU is 84,544,416 ns lower than the baseline median, inside the baseline
210,227,334 ns MAD. It is below both the 7 MB lane target and the
6,110,043-byte provisional CI ceiling. It is reported only as N=1, not promoted
to the N=5 comparison. Two later attempts were rejected before their CPU
windows at load 4.09 and 5.86 against the fixed limit of 4.

An independent one-sample idle A/B used the original 60-second settled startup
surface with no scripted turns:

| variant | physical footprint | lifetime max | lifetime CPU | threads | load spawn/read | accepted |
| --- | ---: | ---: | ---: | ---: | --- | --- |
| baseline | 5,374,360 B | 6,095,256 B | 102,706 µs | 4 | 2.88 / 2.59 | no — `vmmap` denied |
| candidate | 4,506,008 B | 5,947,800 B | 86,875 µs | 4 | 2.41 / 2.41 | no — `vmmap` denied |

The paired settled delta is **−868,352 B (−16.2%)**, which is measurable and
places the candidate at 4.51 MB. A second baseline observation was 5,702,040 B,
but its matching candidate read was rejected at load 11.72 and was not paired.
Other idle attempts were rejected at loads 5.64 and 14.30.

Raw artifacts are outside the worktree under
`/private/tmp/memclient3-footprint-baseline-sixel-retry1` and the corresponding
`memclient3-footprint-candidate-sixel*` directories.

## Peak and latency hold-outs

The M1 RSS sampler synthetic process-tree test passed (40 samples and positive
client/daemon labels). The real exact-size harness remained unmeasured: baseline
N=5 refused to start at load 4.95 and candidate N=5 refused at 5.05, against its
strict `<3` limit. Therefore there is no publishable proof that the one-shot
client peak is not worse. The relevant product paths are headless and do not
execute the changed TUI wordmark or recall code, but source reachability is not
a substitute for the requested peak measurement.

The warm-daemon harness completed 5 warmups and 25 measured turns per shape in
ABBA order for both frozen clients, with zero correctness failures. It rejected
publication because baseline load was 4.21/4.21/4.36 and candidate load was
3.99/3.99/3.99, all above the fixed `<3` proof pin.

| shape | baseline median / MAD | candidate median / MAD | delta | larger MAD | diagnostic |
| --- | ---: | ---: | ---: | ---: | --- |
| single | 40.540 / 1.721 ms | 40.589 / 3.551 ms | +0.049 ms | 3.551 ms | within MAD |
| tool | 57.105 / 3.408 ms | 58.629 / 4.945 ms | +1.524 ms | 4.945 ms | within MAD |

Thus the rejected warm ABBA has no latency-regression signal, consistent with
the fact that runtime selection did not change. It cannot be promoted to the
required accepted no-regression result.

## Correctness and release gates

- Exact tuivirt release bundle: PASS — 14 golden, 7 scroll, 3 memory, 2 shape,
  and 1 legacy render-benchmark test. Render retention remains exactly 6 KiB at
  1k and 50k rows; measured 10k/50k/200k shape and cached-frame gates passed.
- Dedicated tpsfix bundle: PASS — 19 estimator and 12 widget tests.
- `cargo test -p haider-tui --locked`: PASS for the entire crate and every
  integration target, including the new four-test memclient3 matrix.
- CLI unit/runtime tests: PASS, including headless current-thread and full/TUI
  runtime pins. Autospawn tests passed 10/10. `cli_tests` passed 127/127 when
  rerun with its required prebuilt sibling and
  `HAIDER_TEST_SIBLINGS_PREBUILT=1`.
- `cargo clippy -p haider-tui --all-targets --locked -- -D warnings`: PASS.
- Footprint Python compile, self-test, and unit suite: PASS (16/16).
- `cargo run -q -p xtask --locked -- test-count`: PASS; baseline regenerated
  from 4,437 to 4,441 for the four new tests.
- `git diff --check`: PASS. Lane-owned Rust files are rustfmt-clean.
- Workspace `cargo fmt --all -- --check`: pre-existing failure only in
  `schema_changelog_tests.rs`, `tpsfix_widget_tests.rs`, and
  `ui_polish_tests.rs`; none is changed by this lane.
- The repository has no root `run.sh`; the sole match is
  `scripts/qa-gate/run.sh`, which requires `--tier` and `--bin-dir` and has no
  `test` subcommand. Literal `bash run.sh test` was attempted and exited 127
  (`run.sh: No such file or directory`). The full scoped Cargo suites above are
  the applicable test entry points in this tree.

## CI registry and boundaries

- #20/#21/#54: test count only increased; every Cargo build/test used the
  required 8 MiB stack and hermetic lane environment, and available disk was
  checked before each Cargo command.
- #64: candidate `haiderd` is 52,556,160 bytes.
- #79: tuivirt release shape tests retain their existing large headroom.
- #94: no product deadline changed; the test-only 90-second turn deadline is
  derived from the existing headless terminal observation deadline.
- #95: no wait was added while a negotiated product connection is open.
- The CI Sixel budget remains 6,110,043 bytes. Rejected local diagnostics do not
  authorize recalibration.
- Added production `unsafe`: zero. No daemon, OAuth, RPC, or projection-cache
  source changed.

NO_SHIP
