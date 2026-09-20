# v0.0.972 deep-fixture performance re-baseline

Candidate: `fcdd3997081193ab1a7c16cf5ebf79fedb1cc36a` plus this measurement-only
fixture. Product behavior was not changed. The release binaries were built from
`fcdd3997`; the mimalloc arm differs only by the existing
`haider-daemond/mimalloc` feature.

## Gated-lane verdicts

These are prioritization verdicts, not optimization SHIP claims. A later lane
still has to preserve every durability and conformance law and beat its target
in a granted quiet window.

| Lane | Verdict | Measured target and proving rows |
| --- | --- | --- |
| `wall-journal` / WALL-1 | **PROCEED** | At text depths 5/10/30, `store_access` is 9.604/19.411/54.804 ms wall and 8.847/17.419/58.378 ms CPU: fitted per-turn exponents **0.97 wall / 1.06 CPU**. At tool depth 40 the shared store bucket is **122.424 ± 6.921 ms wall / 107.409 ± 1.745 ms CPU**. Gate a candidate on at least **20 ms wall and 15 ms CPU** reduction at depth 40 plus a visibly flatter store slope. |
| `wall-toolpath` / WALL-3+4 | **PROCEED-REDUCED** | Depth-40 `tool_dispatch` is **3.741 ± 0.951 ms wall / 1.137 ± 0.035 ms daemon CPU**, child CPU is 2.067 ± 0.017 ms, and `store_journal` is **6.508 ± 0.260 / 6.486 ± 0.205 ms**. The direct combined wall target is therefore only about **3 ms**, not the earlier assumed 9+ ms. Do not credit this lane for the 122 ms shared store path. |
| `wall-lockdown` / WALL-2 | **PROCEED-REDUCED, attribution first** | `turn_setup` wall is essentially flat (fit exponent -0.02; 18.343 ms at depth 10, 21.350 at depth 30, 24.258 at depth 40), but lockdown has no exclusive phase. Require a causal A/B showing at least **2 ms wall** before retaining the bind/activate rewrite; otherwise de-scope it. |
| `wall-cas` / WALL-5 | **PROCEED-REDUCED, split before rewrite** | Re-verification and journal reads both land in `store_access`; the current trace cannot divide its **122.424/107.409 ms** depth-40 wall/CPU. Add existing-path byte/hash counters or an exclusive CAS phase and require at least **10 ms wall** attributable to re-verification. The fixture supplies three real capture spills and a 288,681-byte depth-40 provider body, so the workload is now adequate; the phase is not. |
| `cold-init` / COLD-1..5 | **PROCEED-REDUCED** | Cold is **80.582 ± 1.168 ms wall / 30.270 ± 0.708 ms CPU**, statistically unchanged from 971's 80.7/32.0. `first_request` + residual still hold **61.233 ms wall / 25.305 ms CPU**. Target **3 ms wall and 1 ms CPU** there; keep spawn, catalog, directory prep, handshake and runtime builder de-scoped as primary work. |

## Qualification and limit

The standard quiet request was held from 07:28 to 07:50 local time. It could
not be granted because a foreign AHRB `tools/bin971/haiderd` remained alive for
more than two hours despite a 600,000 ms idle linger. Per the lane rule, that
process was recorded and not killed. The request was withdrawn so the quiet
service could release the machine-wide gates.

The measurements below are therefore **not quiet-window-qualified** and no
unconditional SHIP conclusion is made. Every matrix began below load 2, but
the recorded within-matrix load ranges were 1.771-4.462 (FREE-1), 1.892-2.346
(FREE-2), and 1.854-2.428 (FREE-3). The isolated cold run stayed at 1.658.
macOS reported no thermal or performance warning. ABBA block contrasts and
their small-N intervals are reported precisely so background drift is not
silently mistaken for an optimization. A clean granted-window rerun is the
remaining qualification step.

Phase probes were ON in every deep arm, so A/B comparisons share their cost.
The earlier turn-1 probe experiment bounded enabling effects at 5.572 ms wall
for a text turn and 8.365 ms for a tool turn; it did not bound depth-40 probe
cost, and no fixed correction is subtracted here.

## Reusable deep fixture

[`scripts/qa-gate/deep_turn_harness.py`](../../../scripts/qa-gate/deep_turn_harness.py)
builds one deterministic native session per run:

- 40 sequential turns in the same session, with checkpoints 1/5/10/20/30/40;
- 10 real `process_exec` calls, exact-once external-effect verification, and
  capture spills at turns 8/24/36 that leave CAS-backed output in history;
- 24 custom providers with eight models each (192 models total), while the
  selected provider is an in-process loopback OpenAI-chat endpoint;
- 24 response chunks on text turns, fragmented SSE bytes (maximum fragment 21
  bytes), and independently fragmented JSON tool arguments;
- a growing prompt and transcript, with first-request bodies growing from
  9,402 bytes to 288,681 bytes; and
- exact integer-nanosecond phase reconciliation for every turn plus native
  client, daemon and reaped-child CPU accounting.

The harness performs a real profile-scoped stop before each independent run,
checks the lock and sockets, confirms the populated catalog through the CLI,
holds daemon PID/generation stable, and stops the owned daemon afterward.
It supports a five-run baseline or repeated ABBA experiments:

```sh
PYTHONPATH=scripts/qa-gate python3 scripts/qa-gate/deep_turn_harness.py \
  --bin-dir target/perf-artifacts/system \
  --experiment baseline --runs 5 --output /tmp/deep-baseline.json

PYTHONPATH=scripts/qa-gate python3 scripts/qa-gate/deep_turn_harness.py \
  --bin-dir target/perf-artifacts/system \
  --experiment free2-msgpack --rounds 3 --output /tmp/free2.json
```

Use `--require-quiet` only after creating the standard request and receiving
the grant; it checks both markers and load <2 before every turn. FREE-3 also
requires `--variant-bin-dir` pointing at a daemon built with the existing
`haider-daemond/mimalloc` feature.

## Warm depth curve

The primary curve pools the identical system/JSON/trace-off control arms from
FREE-2 and FREE-3 (N=12 independent 40-turn sessions). Totals are median ± MAD.
Depths 20 and 40 are small process-tool turns; the other listed depths are text
turns. This shape distinction explains the intentional sawtooth.

| Depth | Shape | Body bytes | Wall ms | Combined CPU ms |
| ---: | --- | ---: | ---: | ---: |
| 1 | text | 9,402 | 42.515 ± 0.666 | 19.087 ± 1.433 |
| 5 | text | 17,045 | 41.534 ± 4.115 | 26.378 ± 1.739 |
| 10 | text, after first spill | 94,852 | 55.183 ± 3.767 | 43.592 ± 0.649 |
| 20 | tool | 113,786 | 123.574 ± 1.677 | 115.594 ± 1.272 |
| 30 | text | 201,407 | 113.784 ± 2.405 | 126.049 ± 1.289 |
| 40 | tool | 288,681 | 258.654 ± 6.898 | 272.443 ± 3.245 |

The next table gives additive **mean** phase allocation in wall/CPU ms over
the same N=12 samples. Means are used because each row then reconciles exactly;
`other+residual` contains all smaller named phases and the residual. Every raw
sample was also asserted in integer nanoseconds.

| Depth | Total W/C | Turn setup W/C | Store access W/C | Provider assembly W/C | Store journal W/C | Tool dispatch W/C | Turn control W/C | Other + residual W/C |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 43.376 / 19.462 | 22.005 / 1.088 | 5.710 / 4.700 | 0.075 / 0.074 | 7.945 / 1.662 | 0 / 0 | 0.217 / 0.625 | 7.423 / 11.313 |
| 5 | 41.734 / 26.033 | 20.074 / 2.631 | 10.000 / 8.699 | 0.115 / 0.115 | 1.090 / 1.103 | 0 / 0 | 1.178 / 1.591 | 9.278 / 11.893 |
| 10 | 60.578 / 43.172 | 24.554 / 2.541 | 19.866 / 17.339 | 2.062 / 2.062 | 1.392 / 1.405 | 0 / 0 | 2.118 / 4.547 | 10.587 / 15.280 |
| 20 | 124.076 / 115.765 | 19.058 / 4.015 | 65.513 / 52.101 | 7.761 / 7.760 | 7.032 / 5.441 | 6.143 / 0.996 | 6.131 / 13.616 | 12.439 / 31.837 |
| 30 | 113.699 / 125.163 | 21.178 / 6.106 | 54.679 / 57.994 | 11.011 / 11.010 | 2.055 / 2.071 | 0 / 0 | 10.917 / 19.277 | 13.859 / 28.706 |
| 40 | 260.338 / 271.730 | 24.687 / 8.155 | 124.757 / 106.806 | 43.448 / 43.447 | 7.322 / 6.598 | 3.877 / 1.144 | 36.937 / 56.796 | 19.310 / 48.785 |

### WALL hypotheses

| Item | Shape and depth-40 absolute cost | Verdict |
| --- | --- | --- |
| WALL-1 uncached journal decode | On comparable text turns 5/10/30, `store_access` grows 9.604 → 19.411 → 54.804 ms wall and 8.847 → 17.419 → 58.378 ms CPU. The fitted exponent is 0.97/1.06: approximately linear cost per turn, hence approximately quadratic cumulative session work. At tool depth 40 the shared bucket is 122.424/107.409 ms median wall/CPU. | **CONFIRMED** as the dominant repeated store-read family. The phase is an upper bound for WALL-1 because WALL-5 shares it. |
| WALL-5 CAS re-verification | The fixture has three capture spills and a 288,681-byte depth-40 body, and the shared store phase grows strongly. There is no CAS-specific counter or exclusive scope, so no honest fraction of the 122.424/107.409 ms can be assigned to re-hashing. | **UNRESOLVED**. The workload is sufficient; attribution is not. |
| WALL-3 duplicate `git status` | Small tool turns show `tool_dispatch` wall falling from 8.288 ms at turn 4 to 3.741 ms at turn 40 (fit exponent -0.41); CPU rises only from 0.840 to 1.137 ms (p=0.12). Reaped-child CPU is 2.067 ms at depth 40. | **CONTRADICTED as a 9 ms late-depth target**, but confirms a smaller constant per-call opportunity. |
| WALL-4 serialized appends | Ignoring the first-turn durability outlier, tool `store_journal` is roughly flat: 4.494-7.352 ms wall and 4.509-6.486 ms CPU, CPU exponent 0.05. Depth 40 is 6.508/6.486 ms median wall/CPU. | **CONFIRMED** as a constant per-tool cost, not a growth source. |

The un-gated `provider_assembly` phase also becomes material: 11.020 ms on
text depth 30 and 43.447 ms on tool depth 40. This is request construction,
not a CAS measurement, and must not be relabeled as WALL-5.

## FREE experiments

Each experiment used three ABBA blocks (`A B B A`), six samples per arm. The
reported interval is a two-sided 95% Student-t interval over the three
block-median B-A contrasts. With only three blocks and a non-quiet host it is a
noise description, not a deterministic bound.

| Experiment at depth 40 | A median W/C ms | B median W/C ms | Paired wall delta, 95% interval | Paired CPU delta, 95% interval | Recommendation |
| --- | ---: | ---: | ---: | ---: | --- |
| FREE-1 store trace off/on | 280.485 / 308.809 | 330.010 / 363.351 | +100.520 [-78.338, +279.378] | +72.016 [+16.022, +128.010] | **Reject as a default; diagnostics only.** It is materially perturbing by deep history. |
| FREE-2 JSON/MessagePack | 255.550 / 268.840 | 248.398 / 266.509 | -7.466 [-25.520, +10.589] | -4.010 [-10.899, +2.880] | **Reject adoption now.** No resolved depth-40 win; depth-20 wall regressed +1.839 [+0.725, +2.954] ms. |
| FREE-3 system/mimalloc | 258.654 / 273.866 | 259.229 / 261.700 | -3.893 [-19.934, +12.148] | -10.509 [-15.610, -5.407] | **Needs more.** Deep CPU is promising (also -6.959 [-9.411, -4.507] ms at depth 30), but wall is unresolved and a quiet RSS/platform rerun is required before adoption. |

FREE-1 does answer where the store work goes. Queue delay stays tiny while
operation work and call count grow. Aggregate operation sums may exceed turn
wall because independent operations overlap; they are work totals, not
critical-path attribution.

| Depth | Records/turn | Queue median us | Queue sum us/turn | Operation median us | Operation sum us/turn |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 126.5 | 5 | 840 | 56 | 23,731 |
| 5 | 110.5 | 5 | 669 | 82 | 31,042 |
| 10 | 119.3 | 5 | 732 | 89 | 58,312 |
| 20 | 312.8 | 5 | 2,592 | 143 | 285,419 |
| 30 | 151.2 | 5 | 1,106 | 304 | 166,360 |
| 40 | 355.0 | 5 | 2,763 | 312 | 532,216 |

At depth 40, queue sum is only 0.52% of aggregate operation sum. Store time is
in operation bodies, not blocking-pool admission.

## Cold re-baseline

The canonical instrumented one-shot harness ran five warmups and 21 fresh
profiles. Median wall is 80.582 ± 1.168 ms and median process-tree CPU is
30.270 ± 0.708 ms. The additive phase table uses means and reconciles exactly.

| Phase | Wall ms/sample | CPU ms/sample | Previous 971 wall ms |
| --- | ---: | ---: | ---: |
| first_request | 32.922 | 8.006 | 31.97 |
| residual | 28.311 | 17.299 | 28.63 |
| store_open | 9.970 | 2.363 | 10.20 |
| teardown | 6.436 | 0.752 | 6.95 |
| spawn | 0.944 | 0.301 | 0.96 |
| capability_catalog | 0.725 | 0.724 | 0.76 |
| directory_prep | 0.680 | 0.623 | 0.69 |
| socket_handshake | 0.241 | 0.126 | 0.26 |
| runtime_init | 0.081 | 0.078 | 0.08 |
| **mean total** | **80.308** | **30.273** | **about 80.7** |

The landed attribution/CPU-correction wave did not materially move cold wall:
the median is 0.1 ms below the quoted 80.7 ms, far inside dispersion. It did
not expose a safe 37 ms cold saving. Cold work should stay restricted to the
large first-request and residual buckets with modest measurable targets.

## Artifacts and reproducibility

Raw reports live outside the repository in
`state/evidence/972-perf-rebaseline/1-measure/`:

- `free1-store-trace.json` (12 sessions / 480 turns),
- `free2-msgpack.json` (12 / 480),
- `free3-mimalloc.json` (12 / 480), and
- `cold-one-shot.json` (5 warmups + 21 measured fresh profiles).

System binary hashes are `haider`
`78abc1ce4d1dd0a039ea16868d39684beae2a8774c36848f1d3e660e08423617`
and `haiderd`
`ebbd82727de334549581bd43cc2fe32fffcd99084420567c9503a8462658d4dd`.
The mimalloc daemon hash is
`95c14d6709cb83d2c9b30ea8cf6f477cfc598d667d76b30a4bd704bd9dc820af`.
No binaries, build cache, CAS tree or APK tree is copied into evidence.

## Local validation

- Full Python QA discovery under the required short `TMPDIR`: 132 passed.
- Deep-fixture self-check, Python compilation and whitespace checks: passed.
- Workspace Clippy with all targets and denied warnings: passed.
- Workspace tests with freshly built runtime siblings and
  `HAIDER_TEST_SIBLINGS_PREBUILT=1`: passed.
- rustfmt: passed; unsafe-count: production 197 / test 21; test census:
  5,713 against the 5,711 minimum.

The initial workspace-test invocation omitted the documented sibling prebuild
environment and eight subprocess targets refused before exercising product
behavior. The exact instructed sibling build and full corrected rerun passed.
