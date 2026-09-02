# v0.0.970 turnhygiene2 lane report

Status: **SHIP**. The two isolated retention regressions were corrected, the
final full set improves same-wave retention and post-40 footprint, and both
halves of the quiet-host A/B/B/A reproduce the accepted wall win.

## Recovery and scope

This continuation recovered the uncommitted worktree after the shared-disk
failure, re-read `LANE-COMMON.md`, the continuation brief, the original brief
from the preceding Codex session log, and the `turnperf/` and `turnperf2/`
evidence. The current branch already contained the required merge of
`origin/wave-970` at `e3fc3f5` (`92482f9` is the merge commit), so no second
merge was made. The recovered source delta is nine tracked files: 714 insertions
and 90 deletions, plus the untracked lane evidence directory. No commit was
created and none of the supplied v0.0.970 evidence was added to Git.

Rust work uses `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, two Cargo jobs, and
prebuilt sibling binaries for process tests. Disk was checked before builds.

## One-item retention bisect

Each row is an isolated item on the then-current clean `wave-970` at `8952219`,
measured with the prescribed 40-turn daemon protocol. Three quiet-host samples
are retained; overload attempts are rejected rather than trimmed. Delta is
candidate minus the clean-wave median of 243,712.6 B/turn, so negative is
better. After the wave advanced to `e3fc3f5`, the final clean baseline was
rebuilt and recounted separately below.

| Item | Median retention | Delta vs clean | Result |
|---|---:|---:|---|
| Clean wave | 243,712.6 B/turn | baseline | N=3 median; idle 5,489,120 B, post-40 15,172,088 B, growth 9,748,504 B |
| R2-12 unbudgeted estimator skip | 216,270.0 B/turn | -27,442.6 B/turn (-11.3%) | Within noise / no regression |
| R2-19 exact turn-start read bundle, original form | 303,514.8 B/turn | +59,802.2 B/turn (+24.5%) | Regression isolated |
| R2-11 memory-first submit race buffer, original form | 287,950.0 B/turn | +44,237.4 B/turn (+18.2%) | Regression isolated |
| R2-15 linear instruction walk + bounded cache | 237,159.6 B/turn | -6,553.0 B/turn (-2.7%) | Within noise / no regression |

R2-19's original N=3 medians were 5,489,120 B idle, 17,629,712 B post-40,
and 12,140,592 B growth. Durable row counts and journal bytes matched the clean
wave, isolating allocator high-water from retaining and decoding the complete
journal in the turn-start bundle rather than durable growth.

R2-11's original N=3 medians were 5,571,040 B idle, 17,105,424 B post-40,
and 11,518,000 B growth. The implementation had landed a 256 KiB transient
race prefix despite the proposal's strict 32 KiB bound. Its raw accepted
retention values were 287,950.0, 289,996.8, and 269,108.4 B/turn.

R2-15's N=3 medians were 5,521,888 B idle, 15,024,656 B post-40, and
9,486,384 B growth. Its raw retention spread (237,159.6, 283,444.4, and
190,874.8 B/turn) brackets the clean wave.

## Root-cause corrections

- R2-11 now enforces the proposed 32 KiB in-memory submit-race byte cap while
  retaining the existing 64-frame cap and spill/replay path. A pin asserts the
  exact cap.
- R2-19 still samples one exact turn-start dispatch, validates every journal
  row, and returns the immutable head, delegation, and graph state, but it no
  longer retains the unrelated decoded journal. It keeps only recognized
  headless facts for the accepted run, folds context-economy events to their
  latest coordinates, and records current-run start metadata separately.
- Turn-setup reduction now reads filtered pages through the sampled immutable
  `(seq, event_id)` boundary and reuses the bounded suffix cache. Later appends
  are excluded; missing or forged boundaries fail closed with `StoreCorrupt`.
  Pins cover the production SQLite boundary, cache advancement, malformed and
  backward context history, and equal-coordinate disagreement.

## Final full-set evidence

The final binaries were independently rebuilt from pristine `e3fc3f5` and the
exact current worktree, then copied to immutable comparison directories. Their
SHA-256 hashes are:

| Artifact | Clean e3fc3f5 | Corrected full set |
|---|---|---|
| `haider` | `a13b10bfe7b2a60cbf5fba0a5fb6cd3dc3e6c969062ffe7f49ed04d6d6b0aec5` | `d2c78b82bc2032c602456dfc750bd56ec6e82932bc2bb2836a74040e8ff18db3` |
| `haiderd` | `111266641e709d5b3c0c7e56149a5a2df3f4009d76207ccb7ef76d89b440585f` | `71525ddd40a7ff9371811f896630a2980523b0edf8f109c8220c9b23d138fded` |
| footprint driver | `c01bb0f648f504fc85da5ca5325a1331d56c039cefaf9883949de361d3125063` | `17ac8ae552879cb9fa178105e59f575f34dbb047bb556fa0e6a8e695a665c5b4` |

### Same-wave N=3 footprint

Both sides used 40 turns, 60-second idle and post-turn settle periods,
retention attribution, and load1m below 4 for every retained sample. One
attempt per side was rejected for overload and is not included.

| Metric | Clean e3fc3f5 median ± MAD | Corrected full set median ± MAD | Delta |
|---|---:|---:|---:|
| Settled idle | 5,571,016 ± 32,768 B | 5,538,272 ± 0 B | -32,744 B |
| Post-40 footprint | 16,286,176 ± 196,632 B | 13,451,792 ± 65,560 B | -2,834,384 B |
| Settled 40-turn growth | 10,715,160 ± 311,296 B | 8,011,824 ± 98,304 B | -2,703,336 B |
| Retention | 267,879.0 ± 7,782.4 B/turn | 200,295.6 ± 2,457.6 B/turn | **-67,583.4 B/turn (-25.2%)** |

Clean accepted retention was 275,661.4, 267,879.0, and 226,509.4 B/turn;
its rejected attempt reached load 5.08. Candidate accepted retention was
200,295.6, 197,838.0, and 240,435.8 B/turn; its rejected attempt reached load
5.37. Durable attribution remained structurally equal across the compared
workload shape. The final full set is therefore better than clean wave on the
retention, post-40, and idle gates rather than merely within noise.

### Final wall A/B/B/A

Each cohort used trace-off 5 warm-ups plus 25 untrimmed retained samples per
shape. All four reports have `measurement_accepted=true`; every start/mid/end
load snapshot was between 1.72 and 1.96, below the required limit of 3.

| Cohort | Single wall / combined CPU (ms) | Tool wall / combined CPU (ms) |
|---|---:|---:|
| A1 clean | 40.782 ± 3.104 / 4.449 ± 0.137 | 61.518 ± 3.272 / 5.260 ± 0.193 |
| B1 full set | 39.025 ± 3.667 / 4.408 ± 0.170 | 58.947 ± 2.224 / 5.227 ± 0.205 |
| B2 full set | 37.577 ± 1.784 / 5.596 ± 0.182 | 57.372 ± 3.704 / 6.712 ± 0.321 |
| A2 clean | 41.879 ± 4.081 / 5.771 ± 0.230 | 58.974 ± 3.686 / 7.073 ± 0.263 |
| Pooled A -> B | **40.795 ± 3.153 -> 38.096 ± 2.721, -2.698** / **5.324 -> 4.895, -0.429** | **60.872 ± 5.070 -> 58.854 ± 3.125, -2.017** / **6.007 -> 5.939, -0.068** |

Both brackets reproduce the candidate wall win. Pooled client peak-RSS medians
move by +88 KiB single and +120 KiB tool; pooled combined peak medians move by
+264 KiB single and -96 KiB tool. Those changes are within run noise, while
candidate maximum combined peaks are lower in both shapes (55,120 vs 55,504
KiB single; 55,872 vs 55,984 KiB tool).

## Correctness and static verification

The deleted build directory was rebuilt from scratch. Debug `haider` and
`haiderd` build cleanly, followed by exact release builds of both comparison
trees. The nine orchestrator behavior pins, the R2-12 estimator pin, all four
R2-19 store pins, the production immutable-boundary pin, both R2-11 submit-race
pins, and R2-15's linear/bounded/loss-detecting cache pin are green.

The exact final source passed the complete affected suites for `haider-core`,
`haider-store`, `haider-client`, `haider-cli`, and `haider-daemon`. The daemon
unit binary reports 924 passed / 3 pre-existing ignored, and session-hub reports
103/103. Scoped `--tests --no-deps` Clippy with `-D warnings` passes for all five
crates. `cargo fmt --all -- --check`, `git diff --check`, the unsafe-count gate
(`production=189`, `test=20`), and all 61 QA-gate harness self-tests pass. The
test-count guard is exact at 4,394/4,394.

The exact candidate also passes the durable crash matrix: 47/47 boundaries,
zero failures, every store-integrity check `ok`, and 55 distinct provider
request coordinates with zero duplicate `(case_id, logical_ordinal)` pairs.

Citation audit: R2-12 is currently anchored at `actor.rs:3550,12441`; R2-19 at
`worker.rs:7252,7645,8845`, `event_store.rs:1922,4542,13053,20305`, and
`sqlite_store.rs:366,2742`; R2-11 at `headless.rs:97,1250`; and R2-15's bounded
cache at `project_instructions.rs:42-44,84,343`. Proposal line numbers drifted,
but the named constructs and ownership boundaries remain accurate.

## CI registry walk

- #64: the exact candidate release `haiderd` is 52,423,808 B, above the 10 MiB minimum.
- #77: the unsafe-count guard passes; the lane adds no production unsafe use.
- #94: no deadline or timeout was added.
- #95: no negotiated-connection wait or keepalive obligation was added.
- No workflow or QA-gate registry source was changed.

## Verdict

All four retained latency items remain. R2-19 and R2-11 now satisfy their
intended bounded-memory designs; the final tree improves same-wave retention by
25.2% and reproduces >2 ms pooled wall improvements in both shapes without a
CPU or peak-RSS regression. No release, commit, evidence commit, workflow, or
OAuth change was made.

SHIP
