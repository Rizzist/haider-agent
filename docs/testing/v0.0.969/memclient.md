# Lane memclient — v0.0.969 client footprint

Date: 2026-09-01  
Branch: `lane-969-memclient` at `d75a8ea`, with the lane changes left uncommitted.  
Compared binaries: installed `/usr/local/bin/haider 0.0.968` and this worktree's
`target/release/haider 0.0.968` (34,615,776 bytes). The sibling release daemon is
52,340,720 bytes. No daemon source file was changed.

## Verdict

The lane is **NO_SHIP**. The two-worker TUI runtime is a positive candidate in the
available diagnostics, but the owner-mandated evidence is incomplete:

1. This execution sandbox permits `proc_pid_rusage` but denies `vmmap` access to
   every live child. Every five-run result below is therefore explicitly rejected
   diagnostic evidence, not a publishable `vmmap -summary` measurement.
2. The fixed 20-turn CPU workload was not completed.
3. The 1.06 MiB reply peak harness produced one valid installed-baseline run, then
   failed to identify its daemon on run two. There is no N=5 before/after peak
   verdict.
4. The terminal-image experiment exceeded the CPU MAD and raised observed peak, so
   it was reverted under the hold-out rule.

The authoritative facts remain the owner's live TUI baseline: **10.6 MB physical
footprint**, **12 threads**, **5,200 KiB `MALLOC_SMALL` dirty**, one **5,920 KiB
`MALLOC_LARGE` region with 2,992 KiB dirty**, and **608 KiB dirty stacks**.
[FACTS.md:9](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/FACTS.md:9)
[FACTS.md:20](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/FACTS.md:20)
The fresh demo surface used below is materially smaller than that live process, so
its approximately 5.8 MB baseline must not replace the owner's 10.6 MB number.

## Citation audit

| Brief/facts claim | Audit | Current evidence |
| --- | --- | --- |
| TUI general mode uses a CPU-sized multi-thread runtime; `run` and `status` are current-thread | **Correct construct, drifted line** | The prior merged table points at old `main.rs:57/80`; current runtime selection is `main.rs:71-94`. `run` and `status` already used the lean profile in the installed baseline. [MERGED.md:114](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:114) [MERGED.md:117](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:117) |
| The single large allocation is most likely the retained terminal wordmark protocol | **Source ownership still plausible; saving attribution not confirmed** | The lens assigned only medium confidence and named the RPC frame decoder as a credible alternative. The isolated wordmark-discard experiment did not recover the expected 2.5–2.9 MB and instead regressed all three available signals. [MERGED.md:148](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:148) |
| The named 32 MiB `PromptHistoryCache` is TUI-owned | **Wrong in `FACTS.md`; corrected in merged L1** | Current source has the 32 MiB cache constant in `haider-core`, but its process owner is `haider-daemon/src/session_hub/mod.rs`; the TUI separately owns an uncapped `VecDeque<PromptEntry>`. Touching the named cache would violate the daemon boundary. [FACTS.md:47](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/FACTS.md:47) [MERGED.md:150](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:150) |
| Client `MALLOC_SMALL` has at least 3.67 MB derived slack | **Correct as a derived ceiling, not a proven purge saving** | The prior allocator selection recovered only 0.28 MB measured. No claim supports treating the entire arithmetic slack as recoverable by `malloc_zone_pressure_relief`. [FACTS.md:39](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/FACTS.md:39) [MERGED.md:158](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:158) |
| TUI live state is at most 1.41 MB allocation within 5.08 MB SMALL dirty | **Correct derived ceiling; component split remains unmeasured** | The merged lens identifies two Ratatui buffers, layout copies, full session projections, and uncapped prompt recall, but explicitly rates the split low-to-medium confidence. [MERGED.md:150](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:150) |

## Implemented candidate

`crates/haider-cli/src/main.rs` now selects:

- a current-thread runtime for `run`, `status`, `sessions`, `session`, `fleet`,
  `events`, daemon control, readiness, and JSON resume wire/control paths;
- an explicit two-worker Tokio runtime for the TUI and other general CLI paths.

Non-wire commands such as self-test, account management, update, version, and
interactive resume deliberately remain on the general profile. Tokio's blocking
pool remains lazy. The tests assert both runtime flavor and the exact worker count.

The file also adds an opt-in, 300-second-bounded
`HAIDER_CLIENT_FOOTPRINT_HOLD_MS` seam. It runs only after a command has completed,
does not alter ordinary invocations, and lets the release harness inspect the
otherwise short-lived `status` and `run` processes after 60 seconds.

## Measurement method and acceptance

`scripts/perf/client-footprint-budget.py`:

- reads `ri_phys_footprint`, lifetime maximum, user+system CPU, RSS, and thread
  count with `proc_pid_rusage(RUSAGE_INFO_V4)` and `proc_pidinfo`;
- runs exactly N=5 in calibration mode, settles for at least 60 seconds, and
  requires one-minute load below 4 immediately before spawn and immediately
  before the read;
- drives a 118x36 PTY and a real Sixel capability exchange, waiting for the real
  image DCS before starting the settle interval;
- uses isolated profiles for wire surfaces, validates the typed status response,
  and requires one successful headless terminal plus exactly one request to the
  loopback OpenAI fixture;
- always terminates probe clients and profile daemons;
- saves `vmmap -summary` beside every sample and rejects calibration or budget
  enforcement if `vmmap` exits nonzero;
- computes a standing budget as `ceil(max(N) * 1.10)` and fails only when the
  sample is at or above that upper bound, so improvements pass;
- provides a pre-run self-test for the Darwin process binding and budget
  arithmetic (CI registry guard #77).

All recorded loads were below 4. All 25 A/B/floor samples reached the requested
60-second settle. Nevertheless, every `vmmap` command exited 255 with a denied
task-port diagnostic. The harness's `--diagnostic-allow-missing-vmmap` mode retained
the `proc_pid_rusage` data while setting `measurement_accepted: false`; calibration
and budget modes refuse that override.

## Settled TUI before/after — rejected diagnostics

Values are exact bytes from `proc_pid_rusage`; MB deltas below use decimal MB.
CPU is lifetime user+system CPU at the settled read. “Peak” is the largest
`ri_lifetime_max_phys_footprint` in the five samples, not the separately required
1.06 MiB-reply RSS case.

| Build/surface, N=5 | footprint min / median / max | CPU min / median / max | footprint MAD / CPU MAD | threads | observed lifetime peak | accepted |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Installed baseline, TUI demo Sixel | 5,030,320 / 5,800,368 / 6,193,584 | 942,130 / 1,022,681 / 1,998,113 us | 393,216 B / 80,551 us | 12 | 6,193,584 B | **no — vmmap denied** |
| Runtime-only candidate, TUI demo Sixel | 4,784,536 / 5,538,200 / 5,554,584 | 904,031 / 927,592 / 935,667 us | 16,384 B / 8,075 us | 4 | 5,620,144 B | **no — vmmap denied** |
| Runtime + discarded wordmark protocol experiment | 4,489,624 / 5,603,736 / 6,750,616 | 1,751,040 / 2,089,432 / 2,985,650 us | 98,328 B / 261,804 us | 4 | 6,783,408 B | **no — vmmap denied; reverted** |

Installed baseline to runtime-only diagnostic delta:

- median settled footprint: **-262,168 B (-0.262 MB)**;
- median CPU: **-95,089 us**, a decrease rather than an increase beyond the
  installed baseline's 80,551 us MAD;
- threads: **12 -> 4** (main, two Tokio workers, and terminal input);
- largest observed lifetime footprint: **-573,440 B (-0.573 MB)**.

Isolating the discarded image experiment against the runtime-only build gives
**+65,536 B median footprint**, **+1,161,840 us median CPU**, and
**+1,163,264 B observed lifetime peak**. The CPU increase is over 140 times the
runtime-only CPU MAD. This is a clear hold-out and the image code was fully
reverted.

## Three-signal verdict per requested lever

| Lever | footprint delta | CPU delta | 1.06 MiB-reply peak delta | hold-out verdict |
| --- | ---: | ---: | ---: | --- |
| Discard terminal-image protocol after render | **+0.066 MB** median, rejected diagnostic | **+1.162 s** median, far above MAD | Required M1 A/B unavailable; settled lifetime diagnostic **+1.163 MB** | **REVERTED — negative** |
| Two-worker TUI; current-thread wire/control paths | **-0.262 MB** median, rejected diagnostic | Settled-read **-0.095 s**; fixed 20-turn A/B unavailable | Required M1 A/B unavailable; settled lifetime diagnostic **-0.573 MB** | **Candidate retained, not mergeable without required evidence** |
| Shrink/cap TUI caches | No implementation or A/B | No A/B | No A/B | **HOLD OUT — null evidence** |
| `malloc_zone_pressure_relief` after transients | No implementation or A/B | No A/B | No A/B | **HOLD OUT — null evidence** |
| Remove eager wire-only initialization | No additional eager client subsystem found; final floors below | No before/after lever | No A/B | **HOLD OUT — installed `run`/`status` were already current-thread** |

The cache lever was not guessed into production: clearing active transcript layout
would force reflow work into later frames; shrinking the two Ratatui buffers is
framework-owned; and byte-capping prompt recall without durable paging changes
visible recall behavior. The named 32 MiB cache is daemon-owned. The allocator
lever was likewise held out because it would add Darwin-specific unsafe FFI and
later refault/latency risk without an accepted positive A/B.

## Wire-only “agent client” floor — rejected diagnostics

These are final target-release floors after one completed command, held for the
same exact 60-second interval. Each headless run reached one successful terminal
and made exactly one provider request.

| Surface, N=5 | footprint min / median / max | CPU min / median / max | MAD footprint / CPU | threads | provisional `1.10 * max` budget | accepted |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| `haider status --json --no-spawn` | 2,392,448 / **2,425,216** / 2,457,984 B | 98,389 / 108,438 / 160,761 us | 32,768 B / 10,049 us | 1 | 2,703,783 B | **no — vmmap denied** |
| headless `haider run` fixture | 2,916,760 / **3,015,064** / 3,096,984 B | 112,878 / 191,768 / 204,834 us | 49,152 B / 13,066 us | 1 | 3,406,683 B | **no — vmmap denied** |

Thus the diagnostic agent-client number is **2.43 MB median for status** and
**3.02 MB median for a completed headless run**. These align with the lens's
1.5–3.0 MB status and 2–4 MB run estimates, but are not publishable measurements
until the same calibration succeeds with `vmmap`. [MERGED.md:114](/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/mem969/MERGED.md:114)

## Required CPU and peak evidence not completed

The fixed 20-turn scripted workload was not run. The settled-read CPU values above
therefore cannot substitute for the owner-required workload verdict.

The inherited exact-size M1 harness sends a 1,114,112-byte assistant reply. Its
first installed-baseline run at load 2.67 produced:

- client peak RSS: 16,580,608 B;
- daemon peak RSS: 59,260,928 B;
- daemon growth from the pre-reply sample: 29,556,736 B;
- two provider requests and a successful terminal.

Run two failed because the sampler found zero live daemon descendants where it
required exactly one, so the harness stopped. N=1 is not evidence, and neither a
baseline N=5 nor candidate N=5 exists. No lever can claim the required peak guard.

## Standing CI guard

`.github/workflows/ship-gate.yml` now has a serial `client-footprint` job on
`macos-15`. The current GitHub-hosted runner reference maps that label to an ARM64
M1 runner, matching the calibration architecture. The job runs guard #77, the
harness self-test, a true release build, the daemon-size guard, and then strict
status/run/Sixel budgets. It uploads every sample and `vmmap` file even on failure.
[GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)

The checked-in values are the exact 10%-headroom arithmetic from the rejected
diagnostic maxima:

| Surface | diagnostic max | configured budget |
| --- | ---: | ---: |
| status | 2,457,984 B | 2,703,783 B |
| headless run | 3,096,984 B | 3,406,683 B |
| Sixel TUI | 5,554,584 B | 6,110,043 B |

They are provisional rather than an accepted calibration. The strict CI execution
will fail, rather than silently pass, if `vmmap` is unavailable or if current
footprint is at/above budget. A lower footprint always passes. Shipment still
requires a successful N=5 calibration on the target runner and replacing any
budget whose accepted maximum differs.

## Verification

- `cargo fmt --all -- --check` — pass.
- `python3 -m py_compile scripts/perf/client-footprint-budget.py` — pass.
- `python3 scripts/perf/client-footprint-budget.py --self-test` — pass; one-thread
  child and positive physical footprint observed.
- Harness negative tests — pass: calibration rejects N<5; budget mode rejects the
  missing-`vmmap` diagnostic override.
- `cargo test -p haider-cli --locked` under the exact lane environment — pass for
  every CLI unit and integration test; zero failures and zero ignored tests.
- `cargo test -p haider-cli runtime_tests --locked` after the Python self-test —
  pass, including all repeated integration embeddings of the runtime tests.
- `cargo test -p haider-tui --lib --locked` — 44 passed, zero failed/ignored.
- `scripts/check-unsafe-counts.sh` — pass, production 188 / test 16; no unsafe code
  added.
- `git diff --check` — pass.
- Release build — pass with the lane environment; `haiderd` is 52,340,720 bytes,
  above registry guard #64's 10 MiB minimum.
- Workflow syntax — checked by inspection; `actionlint` and PyYAML are unavailable
  in this environment.

## CI registry walk and boundaries

- #64: release daemon size checked and passed.
- #71: both the installed baseline binary and worktree release artifact were
  exercised end-to-end.
- #72: native discovery was disabled only for hermetic fixture profiles.
- #74: all measurement profiles were throwaway and removed after each sample.
- #77: Python compile/negative guards and the harness self-test ran before the final
  focused Rust acceptance run; the CI job preserves this order.
- No test was weakened, ignored, or platform-gated.
- No daemon source, OAuth file, parallel-lane file, or owner-protected file changed.
- The common/brief/turnperf evidence files remain unmodified and uncommitted.

NO_SHIP
