# Trace-on stage breakdown — MEASURED 2026-09-01 19:18, quiet window (load 2.2, measurement_accepted=true)
Harness: scripts/qa-gate/turn_wall_harness.py, 25 measured + 5 warmups per shape, wave-969 release build @ 3703746 (pre-warmdef; warm turn unaffected).

## Warm harness, trace OFF (the honest 968 vs 969 comparison)
| build | single wall | single cpu | tool wall | tool cpu |
|---|---:|---:|---:|---:|
| 968 installed | 54.9 ± 2.5 ms | 4.9 ± 0.2 | 76.0 ± 4.6 | 5.9 ± 0.2 |
| 969 (3703746) | 54.3 ± 2.7 ms | 4.9 ± 0.2 | 73.2 ± 4.3 | 5.9 ± 0.2 |
=> PARITY. The +16% CPU seen on the loaded conformance run was load noise. Trace ON costs +3 ms single / +17 ms tool (expected; off-path is inert).

## Where the median single turn goes (trace ON, median turn: client-seen accept->terminal = 36.6 ms; harness wall 57.3 ms)
| from | to | ms | what |
|---:|---:|---:|---|
| (exec) | accept | ~20 (DERIVED: 57.3 − 36.6 − tail) | client exec + profile + connect + Hello + submit, and terminal->exit — NOT instrumented |
| 0.00 | 23.55 | **23.5** | accept -> first journal txn / request_attempt_commit: admission, supervisor spawn, session read bundle (>=5 serial store reads), project-instruction walk, hook discovery, tool catalog, budget estimate, lockdown bind/activate — NOT instrumented; **64% of the daemon-side turn** |
| 23.55 | 29.49 | 5.9 | request_attempt_commit (includes the provider-view CAS F_BARRIERFSYNC ~4 ms on this turn; per-record median 0.88 ms => bimodal) |
| 29.51 | 33.43 | 3.9 | provider_open — almost all of it is the fake proxy's own ledger fsync (benchmark artefact, D3-1) |
| 33.45 | 34.09 | 0.6 | first_byte, sse_decode, provider_stream |
| 34.09 | 36.72 | 2.6 | completion + terminal_commit (8 journal txns/turn total = 6.5 ms summed, mostly overlapped) |

## Consequences for the lanes
- The 23.5 ms pre-request gap is the target that matters; PROPOSAL2's per-row estimates (0.3–1.2 ms) were made without this attribution. Lanes must ADD trace phases inside the gap (worker_spawn, read_bundle, instructions, hooks_discovery, tool_catalog, budget_estimate, lockdown, prompt_assembly) before optimizing, then report per-phase before/after.
- The ~20 ms outside accept->terminal is client/process overhead (family 3 rows R2-11/14/22 + R2-16 + C1 startup) — add client-side phases exec->connect->hello->submit and terminal->exit.
- request_attempt_commit's barrier (R2-24, deferred to 970 as "zero on fresh profile") is NOT zero on warm repeated turns: index-proven blocks recur every turn on the same profile. Re-rank for 970 (or turngap) with the warm harness.
