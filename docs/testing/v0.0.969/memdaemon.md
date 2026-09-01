# v0.0.969 memdaemon lane report

Verdict: **NO_SHIP**. The candidate reaches the settled-idle footprint target and
lowers the 1.0625 MiB reply peak, but it does not satisfy all binding hold-out
rules. In particular, settled-idle CPU rose by more than MAD, the long-session
retention remains 276,481 B/turn, individual early levers were not independently
A/B measured, and this managed host denied `vmmap`/`footprint` task inspection.

## Measurement protocol and binaries

- macOS physical footprint and CPU are read from `proc_pid_rusage` for the exact
  daemon PID. Each admitted settled run waits 60 seconds before idle and post-turn
  samples, uses one 40-turn `process_exec` session, and requires load1m `< 4`.
- Final N=5 artifact: `/tmp/memdaemon-final-n5-settle60.json`. Candidate SHA-256
  `41671d4a0f1c9bddb923b9bb18337cf9a4324dcdb16e4b7e2b88211c3a5d37ff`,
  52,357,392 bytes.
- Controlled comparator N=5 artifact:
  `/tmp/memdaemon-ten-worker-n5-settle60.json`. Comparator SHA-256
  `5a8dfd79ac808c19a504f75199cf6a8a30b67c93b81d88c4abeba8b565f7802c`.
  It contains the recovered SQLite/projection changes and the unbiased fake
  provider, but retains ten runtime workers and lacks delayed idle release and
  the 256-slot rings. One load-4.25 run was rejected and replaced.
- `vmmap -summary` and `footprint` were attempted at idle and post-turn for every
  run. All calls were denied by the managed sandbox with
  `task_read_for_pid: Operation not permitted`; the denial outputs are preserved
  under `/tmp/memdaemon-{final,ten-worker}-n5-artifacts/`. Consequently the lane
  has authoritative `proc_pid_rusage` physical-footprint measurements, but not
  the required vmmap region attribution.
- The supplied prior settled baseline was 10.4 MB. It is useful context, but is
  not treated as an isolated A/B because its workload and fake-provider retention
  differed from this lane harness.

## Before/after process results

All table values are medians; parenthesized values are MAD. MB percentages use
the exact byte values.

| Metric | Ten-worker comparator, N=5 | Final candidate, N=5 | Delta |
|---|---:|---:|---:|
| Ready/liveness footprint | 5,931,536 B (16,384) | 5,521,888 B (114,688) | -409,648 B (-6.9%) |
| Settled idle footprint | 5,931,512 B (49,176) | 5,472,736 B (16,384) | -458,776 B (-7.7%) |
| Immediate turn-40 footprint | 21,692,992 B (196,584) | 20,840,976 B (294,936) | -852,016 B (-3.9%) |
| Settled post-40 footprint | 21,283,368 B (655,360) | 16,515,600 B (327,704) | -4,767,768 B (-22.4%) |
| Settled growth over 40 turns | 15,302,680 B (606,184) | 11,059,248 B (376,832) | -4,243,432 B (-27.7%) |
| Settled bytes/turn | 382,567 B | 276,481 B | -106,086 B/turn (-27.7%) |
| Settled idle RSS | 17,645,568 B (32,768) | 17,022,976 B (16,384) | -622,592 B (-3.5%) |
| Settled post-40 RSS | 42,565,632 B (245,760) | 41,926,656 B (262,144) | -638,976 B (-1.5%) |
| CPU during 60 s idle | 43,637 ns (5,743) | 66,033 ns (16,722) | **+22,396 ns; FAIL, exceeds MAD** |
| CPU through fixed turn 20 | 9,558,693 ns (79,608) | 9,265,622 ns (407,991) | -293,071 ns (-3.1%) |

The final 5,472,736 B settled footprint is 4,927,264 B below the supplied
10.4 MB historical baseline. The final immediate attached/workload footprint is
20,840,976 B, so the evidence does **not** establish the requested <=12 MB
attached target. The post-turn process is settled but the workload driver has
closed its RPC client by then.

The separate settle-0 N=5 runtime/ring A/B is in
`/tmp/memdaemon-{final,ten-worker}-cpu-n5.json`: final idle was 5,472,736 B
versus 5,718,544 B; post-20 was 15,417,848 B versus 16,581,136 B; workload CPU
was +153,195 ns (+0.81%), smaller than both 229,375 ns comparator MAD and
316,868 ns final MAD. That clears workload CPU noise, but it does not cure the
settled-idle CPU hold-out above.

## Exact 1.0625 MiB assistant-reply peak

The existing M1 harness emitted an exact 1,114,112-byte assistant reply, used
the same v0.0.968 CLI and fake HTTP proxy for both daemons, sampled daemon RSS
every 1 ms, and required two provider requests plus the exact delta/item/done
anchors. Five valid samples per side had load1m `< 4`. Two additional runs were
discarded because the sampler missed the short daemon event window; they never
produced summaries.

| Peak RSS | Baseline | Final candidate | Delta |
|---|---:|---:|---:|
| Median (MAD), N=5 | 56,852,480 B (1,064,960) | 50,741,248 B (2,736,128) | -6,111,232 B (-10.8%); PASS |

Baseline summaries are under `/tmp/memdaemon-m1-baseline*/`; final summaries are
under `/tmp/memdaemon-m1-candidate/`. No assistant-text copy path was changed.

## Lever verdicts

The three requested verdict columns are footprint / CPU / peak. `Not isolated`
is a hold-out failure, not an assumed win.

| Lever | Footprint delta | CPU delta | Peak delta | Hold-out verdict |
|---|---|---|---|---|
| SQLite boot release for spawned daemons; cache `-512` KiB; statement cache 64 | Combined final reaches 5,472,736 B idle; individual delta not isolated | Not isolated | Combined -6,111,232 B | **HOLD**: implementation/tests green, measurement rule unmet |
| Lazy shared HTTP/TLS client | No change | No change | No change | **HOLD**: required change enters `oauth.rs`, explicitly forbidden by lane common |
| Graph reductions and graph telemetry | Removed retained raw-envelope vector; telemetry continuation drops on idle; individual delta not isolated | Not isolated | Combined -6,111,232 B | **HOLD**: behavior pins green, per-lever measurement missing |
| Delayed idle prompt/setup/observe eviction + SQLite release + libmalloc pressure relief | Controlled combined post-40 -4,767,768 B; -106,086 B/turn | Turn-20 -293,071 ns, but settled idle +22,396 ns beyond MAD | Combined -6,111,232 B | **REJECT/REWORK** under binding idle-CPU rule |
| Mimalloc feature A/B | Not implemented or measured | Not measured | Not measured | **HOLD**: no evidence, therefore not adopted |
| Four Tokio workers + two 256-slot wake rings | Settle-0 controlled post-20 -1,163,288 B; idle -245,808 B | +153,195 ns, within both MADs | Combined peak improved, not isolated | **HOLD**: peak not isolated per lever |
| Usage-history runtime/lazy blocking pool | No change | No change | No change | **HOLD** as null lever |
| Supervisor/actor release after close | Existing retirement/delete suites pass; no lane delta | No lane delta | No lane delta | Existing behavior retained; not claimed as this lane's measured win |

## Long-session retention and measurement seam

The injected `FakeProvider` previously cloned every complete `TurnRequest` into
an inspection vector. Because each request contains the growing same-session
conversation, this test-only ledger falsely reported 764-822 kB/turn. The fake
provider now has an explicit no-recording mode used only by the daemon's injected
test environment; normal tests still record requests and the new mutation pin
checks that both modes work.

With that contamination removed, the controlled comparator retains 382,567
B/turn and the candidate retains 276,481 B/turn after a 60-second settle. The
candidate drops journal-reconstructible prompt bodies, turn-setup reductions,
exact-head observe folds, graph telemetry continuation, and allocator dirty
pages. This is a real 27.7% improvement, but it remains above the supplied
~220 KiB/turn baseline and is not flat. Per-structure growth counters for effect
records, ledger rows, CAS refs, and journal indexes were not completed, so the
remaining 276,481 B/turn cannot be honestly attributed. This is independently
NO_SHIP for item 6.

## Standing CI guard

`scripts/perf/daemon-footprint-budget.py` runs the liveness-spawned release
daemon for N=5, 40 turns, 60-second idle/post-turn settles, rejects samples with
load1m >=4, records CPU/RSS/physical-footprint checkpoints, and fails only when
the median exceeds an upper bound. Defaults are exactly 1.10x the final medians:

- settled idle: 6,020,010 B;
- settled post-40: 18,167,160 B.

Lower footprints always pass. `.github/workflows/ship-gate.yml` builds the true
release daemon and deterministic driver, runs the guard in its own macOS job,
and uploads JSON plus vmmap/footprint reports. A local default-budget smoke passed.

## Functional verification

- `cargo fmt --all -- --check`, `git diff --check`, Python byte-compilation,
  scoped all-target Clippy with `-D warnings`, and `cargo run -p xtask -- check`
  passed.
- Unsafe-count gate passed at production=189/test=16 after the reviewed macOS
  libmalloc FFI changed `haider-platform` production 104 -> 105.
- Full affected suites passed serially: `haider-platform`, `haider-protocol`,
  `haider-store`, `haider-core`, `haider-provider`, `haider-daemon`,
  `haider-daemond`, and `haider-client`. Existing live/manual gated ignores were
  unchanged.
- Daemon alone passed 910 unit tests plus 103 session-hub integration tests and
  smoke/state-machine tests. Daemond's real UDS, process lifecycle, cancellation,
  OAuth, and live-turn fake-provider suites all passed.
- Named mutation pins cover 4 workers, 256-slot rings, no-record fake requests,
  prompt replay after idle eviction, exact-head observe eviction, per-session
  turn-setup eviction, graph projection-only retention, telemetry idle drop, and
  SQLite's cache-size pragma.

## CI error registry walk

No new registry class was discovered.

| Class | Result | Evidence |
|---:|---|---|
| 1/2/3/6/9/10/11/39 | checked | Exhaustive affected suites and scoped all-target deny-warnings Clippy pass. |
| 5/18/44/45/77 | fixed | macOS FFI is target-scoped, has a smallest-scope SAFETY comment, count baseline 104 -> 105 is reviewed, and unsafe gate passes. Non-macOS arm is safe no-op by inspection. |
| 8/19 | checked | Final diff reread; formatting and `git diff --check` pass. |
| 20 | fixed | Repository test baseline reports 4,333 tests versus 4,324 baseline. |
| 21/54/67 | fixed | Every test used the required 8 MiB stack; daemon tests used prebuilt siblings. |
| 23/24/27/31/52/66/72/76 | checked | No schema, provider authority, Windows wire, Android, TUI, STT, credential-discovery, or wire-field change. |
| 25/33/42/74 | fixed | N=5 isolated PID measurements report median/MAD and load; no launch stopwatch claim; temporary HOME/profile only. |
| 32/70/78 | checked | No release/tag/dispatch action was performed. The ship-gate workflow gains only a standing measurement job. |
| 34 | checked | No new dependency; allocator FFI uses the already-locked `libc` crate. |
| 41/46/51/53/60 | checked | No endpoint/root/profile-lock/permission/connection-liveness behavior change. |
| 48/61 | fixed | Tests stay in declared modules and every implemented retention/ring/runtime claim has a named mutation pin. |
| 64 | fixed | Measured release daemon is 52,357,392 bytes, well above the 10 MiB truncation floor. |
| 71 | fixed | Release candidate was executed in footprint, CPU, peak, and budget smoke workloads. |
| 94 | checked | Five-second idle release delay is a cache policy, not a protocol deadline; no shorter outer deadline was introduced. |
| 95 | checked | Measurement sleeps keep launcher liveness but do not leave a negotiated RPC connection waiting on external state. |

All other registry classes were read against the final diff and were not
applicable. The binding evidence gaps and CPU failure above remain; green tests
do not override them.

NO_SHIP
