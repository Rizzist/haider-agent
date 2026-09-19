# Darwin warm-turn CPU accounting correction (972)

**Offline reconversion of existing saved samples, not fresh measurement.** Original benchmark JSONs and reports are unchanged. This restatement supersedes their warm CPU columns only. It does not establish new performance qualification or a product speedup.

The saved daemon field was `(ri_user_time + ri_system_time) / 1e6`, but those counters are Mach absolute ticks. The corrected runtime sampler queries `mach_timebase_info`: `ns = ticks * numer // denom`, then converts ns to ms. This original Mac mini reports 125/3. That ratio is unit metadata observed on the original host; the legacy reports did not save it. Another machine must not supply its own timebase when reconverting these samples.

For each saved row: **corrected client + daemon self ms = client_cpu_ms + daemon_cpu_ms × 125/3**. Compute median/MAD and total from those per-sample sums. The client getrusage value is already milliseconds and is unchanged. Do not multiply the old combined total or sum component medians.

| Saved run / contender / shape | n | Old total ms | Old median ms | Corrected total ms | Corrected median ± MAD ms |
|---|---:|---:|---:|---:|---:|
| Pre-tag quiet / 971 / single | 25 | 103.142 | 4.125 | 460.572 | 17.825 ± 1.649 |
| Pre-tag quiet / 971 / tool | 25 | 126.010 | 5.052 | 744.200 | 28.517 ± 1.689 |
| Pre-tag quiet / 970 / single | 25 | 100.615 | 3.983 | 436.353 | 17.319 ± 1.823 |
| Pre-tag quiet / 970 / tool | 25 | 124.772 | 4.985 | 718.968 | 28.871 ± 2.136 |
| WIP current-load / 971 / single | 25 | 104.690 | 4.105 | 466.848 | 17.522 ± 1.689 |
| WIP current-load / 971 / tool | 25 | 124.868 | 4.965 | 733.025 | 29.060 ± 1.993 |
| WIP current-load / 970 / single | 25 | 100.795 | 4.009 | 434.860 | 17.550 ± 2.141 |
| WIP current-load / 970 / tool | 25 | 122.889 | 4.900 | 705.891 | 27.193 ± 1.878 |

**These reconstructed totals exclude daemon-reaped tool/child CPU.** The old reports never saved those counters; no offline calculation can recover them. The corrected JSON intentionally reports that historical component as null, not zero. The sampling interval was before CLI launch through after CLI exit, **before** `wait_session_idle` and status probes. Prior reports claiming the idle observation was included were inaccurate. Later daemon work and fake-provider/harness CPU are excluded.

The 971-versus-970 comparison also changes:

| Run / shape | Corrected total delta (971 − 970) | Corrected median delta |
|---|---:|---:|
| Pre-tag quiet / single | +24.219 ms (+5.55%) | +0.506 ms (+2.92%) |
| Pre-tag quiet / tool | +25.232 ms (+3.51%) | -0.354 ms (-1.23%) |
| WIP current-load / single | +31.989 ms (+7.36%) | -0.028 ms (-0.16%) |
| WIP current-load / tool | +27.134 ms (+3.84%) | +1.867 ms (+6.87%) |

The pre-tag tool median changes from a small reported increase to a 0.354 ms decrease; its total still increases. The WIP single median also changes sign. These descriptive changes do not establish statistical significance.

## What stays valid

- **971 versus rick addition-G CPU is unchanged:** pre-tag common-nine CPU is 576 versus 424 ms, +152 ms (+35.8%); WIP is 434 versus 376 ms, +58 ms (+15.4%). Both contenders use the same conformance `os.wait4` path, whose `ru_utime` and `ru_stime` seconds are multiplied by 1000. Neither receives the Mach factor. Rick has no saved warm persistent-daemon row, so no warm CPU comparison with rick can be inferred.
- **One-shot complete-lifecycle CPU is unchanged:** the authoritative `process_tree_cpu_ms` is the harness parent's `getrusage(RUSAGE_CHILDREN)` delta. The CLI waits for its owned TTL=0 daemon (`crates/haider-client/src/headless.rs`, `reap_owned_daemon`), and the harness waits for the CLI, propagating waited descendant usage. This inherited counter brackets all children reaped by the harness, including any helper it launches; it is not a new per-PID wait4 measurement. Pre-tag 971/970 totals remain 709.525/727.558 ms; WIP totals remain 687.917/663.700 ms. `thinexe_abba` exec-floor wait4 CPU and `casstream_bench` getrusage/wait4 CPU are also unaffected.
- **Native sampled-client CPU lower bounds are affected in both warm and one-shot modes** (and consumers such as thinexe ABBA): they use the same `_process_usage` sampler. Warm saved lower bounds are also reconverted in the offline JSON. Authoritative client CPU stays unchanged. RSS, physical-footprint bytes, wall time and trace timestamps receive no multiplier.

## Implemented accounting and regression proof

`turnperf_support.py` queries and caches the host timebase, uses integer conversion, and returns daemon self and reaped-child CPU separately. `daemon_cpu_ms` remains self-only; `daemon_reaped_children_cpu_ms` is new; `combined_cpu_ms` sums client + self + reaped children. Report `cpu_accounting.version=2` describes the sources, timebase, interval and exclusions. Missing native CPU counters and regressing deltas fail instead of falling back to a coarse ps value or zero.

On Darwin the child source is `ri_child_user_time + ri_child_system_time`; XNU accumulates the reaped child's own and descendant CPU. On Linux it is `/proc/<pid>/stat` cutime/cstime, converted using `SC_CLK_TCK`. The process tool awaits `child.wait()` before publishing its result. A fresh, exclusive harness daemon serializes cases, so the child delta is attributable to that case's reaped work. It is not per-tool accounting under concurrent unrelated work, and does not include detached/unreaped descendants.

Both warm and one-shot harness entry points run a mandatory synthetic non-unit-timebase check and, on Darwin, compare the actual native samplers with getrusage for the same process. This checks consistency rather than benchmark speed. `python3 scripts/qa-gate/turn_wall_harness.py --self-check-cpu` exercises it without application profiles. Tests cover fake ratios, rounding/large ticks, unavailable APIs, reaped-child wait4 agreement and counter regression. Replacing the real converter body with `return ticks` in a disposable source copy makes both unit tests and this CLI fail.

This supersedes unlanded `8c6e4b40ba2e4d54555e2180e6dd1881914340a8`: its reviewed runtime timebase correction, RSS invariance and native agreement checks are retained in substance, with integer conversion, child accounting and mandatory failure checks added. No unrelated content from that branch was imported.

## Reproduce the offline restatement

Run from the repository with the timebase of the **original sampling host** supplied explicitly. The helper accepts only the audited legacy sampler hash and refuses one-shot, non-Darwin, corrected or unknown-method reports. It emits new JSON to stdout and never rewrites its inputs.

```sh
python3 scripts/qa-gate/reconvert_turn_cpu.py --timebase 125 3 \
  /Users/rizzist/Developer/haiderharness/state/bench-971final/warm-{971final,970-installed}.json \
  /Users/rizzist/Developer/haiderharness/state/bench-971wip2/warm-{971wip2,970-installed}.json \
  > offline-reconversion.json
```

| Original saved input | SHA-256 |
|---|---|
| `state/bench-971final/warm-971final.json` | `8215f79c821bdab61d6af7c7739c251f52ea1ed86c2f7407367c69f5f36f26fe` |
| `state/bench-971final/warm-970-installed.json` | `4db21c4f04efff1956aa7cbb9b6d1d14ba56075e1e8c299d8d259919f8bad19a` |
| `state/bench-971wip2/warm-971wip2.json` | `7fbb4a7392fb86e0b580a04df0ec7702512508a830c0ec9c059813cec358a736` |
| `state/bench-971wip2/warm-970-installed.json` | `8f56bcb52ffbadf8494e63b3cfba1c0d8beec0e31fee0007cf1018b133e72f97` |

Original reports: `state/evidence/971-bench-final/2-run/result.md` and `state/evidence/971-bench2/1-run/result.md`. Implementation evidence, exact commands, runtime metadata, offline rows, original source hashes and mutation logs: `state/evidence/972-fix-cpuunits/1-fix/` (outside Git).

## Independent source audit and remaining diagnostic scope

Apple XNU's [CPU-accounting overview](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/recount.md) distinguishes the interfaces. In [kern_resource.c](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c), `calcru` converts absolute times to timeval before getrusage, whereas `update_rusage_info_child` and `proc_get_rusage` retain absolute child counters. Native self/child agreement tests independently verify the interpretation on this mini. The conformance runner SHA-256 matches the saved method manifest; it is external to this repo and unchanged.

Additional audit finding outside the assigned turn-harness change: `scripts/perf/daemon-footprint-budget.py`, `client-footprint-budget.py` and `darwin-region-snapshot.c` also export raw rusage CPU under ns/us labels. Their CPU diagnostics are not inputs to the tables above and should not be interpreted as those units until a separate footprint-sampler correction. Their RSS/footprint counters and memory budgets remain byte-based. This lane does not certify those diagnostic CPU fields or rewrite historical footprint data.
