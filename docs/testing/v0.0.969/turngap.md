# Lane turngap — accept-to-request and client-envelope attribution

## Verdict

This lane implements attribution and trace infrastructure only. It does not
ship a product-path optimization: the measured dominant phases are owned by
the two parallel lanes, while every unowned candidate fails the hold-out
threshold. No durable boundary, replay rule, wire contract, or provider
request body was moved or changed. The final lane verdict is `NO_SHIP` because
the footprint gates failed and no load-qualified current-build trace-off
harness comparison could be admitted.

The tree has no `turn_wall_harness.py --one-shot` mode at base
`f402fe770346145eaf733cbf35eafb36e1612ba4`; this report therefore uses the
required warm-daemon harness only. The lane brief's older base and line-number
citations have drifted, so every construct was located again in this tree.

## Citation audit

| supplied citation family | result | audit |
|---|---|---|
| R2-10/R2-15/R2-19 worker setup (`worker.rs:7244`, `:7580`, `:7614` in the supplied analysis) | drifted | The constructs remain, now in `start_turn` around the read, lockdown, and instruction blocks; the old numbers were not used. |
| R2-12 actor estimate/guard (`actor.rs:3549`, `:3781`) | drifted | Both constructs remain in the provider-request loop; current locations were found by symbol and surrounding comments. |
| R2-11/R2-14/R2-16/R2-22 client paths (`headless.rs:1203/1276/2843`, `run.rs:553/737`, `main.rs:167`) | drifted | The submit buffer, output adapter, profile resolver, and headless runtime-disposal constructs remain; old line numbers no longer match. |
| R2-20/R2-24 provider rendering and CAS barrier citations | drifted but semantically correct | The encoder/CAS constructs remain. They were attribution boundaries only; neither deferred change was implemented. |
| PROPOSAL2 section 5 rejected mechanisms | correct | Fake-proxy fsync removal, RPC/transaction fusion, unproven CAS-boundary removal, polling, default MessagePack, native-CPU, TLS, and allocator-pressure ideas remain rejected here. |
| wrong citations relied upon | none | No supplied line number was trusted without relocating its construct. |

## Measurement contract

- Apple arm64. The accepted attribution run used release `haider` SHA-256
  `b18ad9491dadc9b359545e0209196047c4d9047604bac4fca21b0148be61afaa`
  and `haiderd` SHA-256
  `9341c26e8cda0a1bbd4d8f91c96b652910d40dddc3598bca3b55cbc3d32b554e`.
- Exactly 5 warmups plus 25 measured turns per shape, alternating ABBA shape
  order, with every sampled one-minute load below 3.0.
- Values are untrimmed per-turn medians ± median absolute deviation (MAD).
  Repeated records such as read bundles and tool-loop request phases are summed
  inside each turn before the median.
- Client and daemon coordinates use independent monotonic clocks. They are
  never subtracted across processes. `submitted` is bounded writer-queue
  admission, not a claim that the kernel write has completed.
- `hooks_discovery` measures the real asynchronous
  `batch_discovery_context` call when it runs. It is not part of the serial
  accept-to-provider-entry sum, and an empty-hook turn may omit it honestly.
- The synchronous daemon trace subscriber is excluded between serial phase
  cursors where possible and becomes explicit unattributed residual. Nested
  journal telemetry can still perturb small trace-on phases, so no marginal
  sub-millisecond phase is treated as an optimization result.

## Before: coarse trace lens

The supplied pre-lane trace could only partition the warm single turn this
far. This is the “before attribution” table, not a product before/after speed
claim.

| interval | median | interpretation |
|---|---:|---|
| client process outside accept-to-terminal | ~20 ms derived | exec/profile/connect/Hello/submit and terminal-to-exit were uninstrumented |
| accept to first request-attempt transaction | **23.55 ms** | 64% of daemon-side single turn was uninstrumented |
| request-attempt commit | 5.94 ms | includes recurrent provider-view/index barrier work |
| provider open | 3.92 ms | mostly fake-proxy ledger fsync benchmark cost |
| first byte/decode/stream | 0.64 ms | provider stream processing |
| completion to terminal | 2.63 ms | eight journal transactions per single turn, mostly overlapped |

## After: detailed phase attribution

Client phases are consecutive intervals: each row ends at the named marker.

| client phase | warm single (us) | warm tool (us) |
|---|---:|---:|
| `exec_start` | 0 +/- 0 | 0 +/- 0 |
| `profile_resolved` | 89 +/- 5 | 89 +/- 6 |
| `connected` | 85 +/- 5 | 83 +/- 6 |
| `hello_done` | 104 +/- 6 | 104 +/- 7 |
| `submitted` | 16,592 +/- 1,821 | 17,234 +/- 1,220 |
| `terminal_seen` | 42,908 +/- 2,280 | 76,375 +/- 4,754 |
| `exit` | 75 +/- 12 | 83 +/- 15 |
| **client `exec_start` to `exit`** | **59,663 +/- 2,990** | **94,316 +/- 5,284** |
| client process residual | 4,550 +/- 273 | 4,435 +/- 337 |

Computed per turn before aggregation, exec through submit is
16.869 +/- 1.769 ms single and 17.486 +/- 1.228 ms tool. The full interval
outside submit-to-terminal is 21.907 +/- 1.666 ms single and
21.848 +/- 1.245 ms tool, matching the earlier approximately 20 ms client
envelope. This derived total includes the parent-process residual; it does not
subtract the independent client and daemon clocks.

Daemon rows are contiguous setup/request spans unless otherwise noted. The
two `read_bundle` records and repeated tool-request records are summed within
each turn before aggregation.

| daemon phase | warm single (us) | warm tool (us) |
|---|---:|---:|
| `accept` | 0 +/- 0 | 0 +/- 0 |
| `worker_dispatch` | 42 +/- 8 | 42 +/- 7 |
| `worker_spawn` | 56 +/- 14 | 63 +/- 18 |
| `read_bundle` (two records) | 4,568 +/- 791 | 4,846 +/- 1,760 |
| `delegation_context` | 49 +/- 9 | 42 +/- 8 |
| `graph_context` | 45 +/- 11 | 40 +/- 6 |
| `provider_resolution` | 39 +/- 4 | 38 +/- 3 |
| `lockdown` | **17,519 +/- 1,629** | **16,566 +/- 1,480** |
| `instructions` | 361 +/- 37 | 397 +/- 55 |
| `prompt_assembly.setup` | 265 +/- 20 | 258 +/- 31 |
| `tool_catalog` | 66 +/- 4 | 65 +/- 3 |
| `setup_finalize` | 65 +/- 5 | 67 +/- 7 |
| `prompt_assembly.request` | 561 +/- 46 | 3,321 +/- 1,761 |
| `budget_estimate` | 13 +/- 1 | 31 +/- 4 |
| `request_prepare` | 113 +/- 8 | 254 +/- 18 |
| `budget_enforcement` | 0 +/- 0 | 0 +/- 0 |
| `request_attempt_commit` | **4,500 +/- 369** | **8,290 +/- 1,682** |
| `provider_open` | 884 +/- 306 | 4,630 +/- 1,999 |
| **accept to first `provider_open` start** | **31,459 +/- 1,650** | **31,010 +/- 2,098** |
| accept to first `provider_open` end | 33,446 +/- 3,090 | 34,450 +/- 2,988 |
| **unattributed serial residual** | **3,014 +/- 1,303** | **945 +/- 570** |

Computed together per turn, `lockdown`, the two `read_bundle` spans, and
`request_attempt_commit` account for 26.470 +/- 1.491 ms on the single shape.
Their median per-turn share of accept-to-provider-entry is 82.80% +/- 3.15%.
`lockdown` plus the read spans account for 21.394 +/- 1.616 ms before the
attempt transaction itself. The remaining explicit setup and request phases
are individually small; the residual includes synchronous trace publication
overhead deliberately excluded from the next phase's cursor.

`hooks_discovery` was absent in the accepted artifact. Independent verification
then found that the frozen binary timed a later cache hit instead of the first
metadata discovery and retained trace-only state on `Retain`. The final source
now instruments the first real call, removes all retained coordinates, and has
named tests for actual discovery, one-shot completion, and cleanup. That
post-evidence correction was not remeasured under load below 3, so the absence
in this table is not treated as a zero or as proof of the corrected phase.

`daemon_accept_to_provider_open_start` ends at the start coordinate of the
first `provider_open`; it excludes fake-provider response-open latency. The
daemon unattributed row is an overlap-safe interval-union residual, excluding
asynchronous hook discovery. `client_process_residual` is Popen wall minus the
client's same-process exec-to-exit coordinate; a negative residual is a hard
correctness failure.

## Ownership and hold-out decision

| measured concentration | attribution and decision |
|---|---|
| client Hello-to-submit, 16.59/17.23 ms | Remaining bootstrap/admission envelope belongs to oneshotboot R2-03/05/09/18; profile/sink work belongs to turnhygiene R2-22. Reported here, not duplicated. |
| `lockdown`, 17.52/16.57 ms | Dominant daemon setup phase; owned by oneshotboot R2-10. Reported with phase evidence, not duplicated. |
| `read_bundle`, 4.57/4.85 ms | Owned by turnhygiene R2-19. Reported with both measured bundle spans, not duplicated. |
| `instructions`, 0.36/0.40 ms | Owned by turnhygiene R2-15. Not duplicated. |
| `budget_estimate`, 0.013/0.031 ms | Owned by turnhygiene R2-12. Not duplicated. |
| `request_attempt_commit`, 4.50/8.29 ms | R2-24 is explicitly deferred to v0.0.970 and requires power-loss proof. Moving this durable boundary is prohibited in this lane. |
| tool `prompt_assembly.request`, 3.32 ms total | This is a two-request, trace-perturbed aggregate. R2-20 requires encoder-only attribution of at least 0.2 ms/request; that threshold was not established, so no rendering change qualified. |
| worker/provider/setup/catalog phases | Each single-turn median is at most 0.27 ms, apart from 0.56 ms request prompt assembly; none clears the hold-out threshold. |
| client `exit` plus process residual, 4.51--4.62 ms | Sink/profile and teardown work belongs to turnhygiene R2-11/14 and oneshotboot R2-16/C1. Reported, not duplicated. |

The hold-out rule therefore rejected a product change. There is no build A/B
ABBA optimization trial to report: none was eligible. The accepted frozen-base
trace-off evidence is shown below for context; it is not a current-build
instrumentation hold-out.

The accepted base trace-off run is recorded below. A current-build run could
not be admitted during the final evidence window because the shared host load
remained above 3.0; the rejected run is not used as evidence. Therefore the
named tests establish disabled-path clock/allocation suppression, but the
requested end-to-end current-build trace-off harness proof remains unmet.

| trace-off base shape | wall median +/- MAD | CPU median +/- MAD | peak RSS |
|---|---:|---:|---:|
| warm single | 57.036 +/- 3.473 ms | 4.930 +/- 0.303 ms | 55,056 KiB |
| warm tool | 78.279 +/- 4.816 ms | 5.928 +/- 0.418 ms | 55,888 KiB |

The accepted base run passed correctness and trace silence with all load
samples below 3.0. Its peaks are also direct evidence that this host/base
combination already exceeded the absolute 51.2 MiB cap.

By named unit tests, instrumentation after the cached environment gate reads no
trace clock and allocates no trace context: the client object is not
constructed and daemon phase closures do not run. The harness scans every
client stderr and daemon log, but only the frozen base obtained an accepted
load-qualified trace-off run; this is why deliverable 1 is not claimed
complete.

## Contract, recovery, and footprint proof

- The explicit client runtime drop only defines the `exit` timing seam. It is
  not the R2-16 teardown optimization and changes no daemon ownership rule.
- Trace fields are static allow-listed phase labels and numeric ordinals or
  coordinates. No prompt, path, session ID, run ID, tool argument, or secret is
  emitted.
- Contract/replay parity is unchanged by inspection and by all affected-crate
  tests. The only hook change retains an in-process trace context until actual
  asynchronous discovery and then purges raced coordinates; hook delivery and
  durable rows are unchanged.
- The SIGKILL matrix passed 47/47, with zero failed cases.

The brief's 5.42 "MB" idle threshold is implemented as 5.42 MiB
(5,683,282 bytes). The accepted canonical footprint run passed it at
5,456,352 bytes, but failed settled growth: 243,713 bytes/turn
(238.001 KiB/turn) versus the 191 KiB limit. The accepted trace-off base
harness peaks were 55,056 KiB single and 55,888 KiB tool, both above 51.2 MiB.
Trace-on peaks are intentionally excluded because tracing itself perturbs RSS.

These are hard lane gates. The trace code has named disabled-path proofs and
does not create its context when off, but the available accepted evidence
cannot prove the required absolute footprint or a non-regression comparison.
Consequently this lane is `NO_SHIP` even though functional, replay, and
SIGKILL verification passed.

## Verification

- Full affected crates: `haider-provider`, `haider-core`, `haider-daemon`,
  `haider-daemond`, `haider-client`, and `haider-cli` passed.
- Scoped `cargo clippy --all-targets -- -D warnings` passed for those crates.
- `bash scripts/qa-gate/run.sh test`: 55/55 passed. The requested repository
  root `run.sh` does not exist; this is the actual QA entry point.
- Named trace tests cover trace-off clock suppression, one-shot setup cursor
  behavior, client marker monotonicity/once behavior, second-accept ordinal
  binding, actual hook discovery/emission/cleanup, telemetry content allow-list,
  interval-union/provider-start cutoff, prompt setup/request separation,
  clock/duration validation, and the client exit seam.
- Test-count baseline updated from 4,351 to 4,359.
- Rustfmt and `git diff --check` pass; no test was weakened, newly ignored, or
  platform-gated.

## Evidence artifacts

The release artifacts below predate the final verifier-driven, trace-only hook
cleanup. That correction changes neither durable rows nor product behavior and
passed focused tests plus final scoped Clippy, but the artifacts are not
final-source-exact. This is an additional reason the lane remains `NO_SHIP`.

- Accepted trace-on attribution: `/tmp/turngap-trace-formal.json`, SHA-256
  `082fc6cce84cfd2cdc4330fe5a86c79d688fb291893de007f8932e4eb82b5a1e`.
  The artifact predates the verifier-driven hook seam correction described
  above; all other reported phase seams are unchanged.
- Accepted frozen-base trace-off run: `/tmp/turngap-off-base-a1.json`, SHA-256
  `45c39ae3e0cb1958b67795be3964b8695dcc596e57698a8bad4143c4773f3010`.
- SIGKILL matrix: `/tmp/turngap-sigkill.json`, SHA-256
  `f39055fb644b31215a010cdae20c4f49e2e62a1ce8296a3c7ed35f0195912773`.
- Canonical footprint run: `/tmp/turngap-footprint.json`, SHA-256
  `8c926cac6cb37bf7b5e3415d42ca637776937c1bc639a12aa9700cfc793dfcfb`.
- Frozen base binaries: `haider`
  `e560dc76f71eec4394a3348532069416a04d404e8d3af6e76597405f3d642326`;
  `haiderd`
  `2a56cbf652ff329379580a93aebe0fcd92987723c5953b4f5cdaec2758eb6ee2`.

All artifacts are local measurement evidence and are intentionally not part
of the repository diff. The supplied `LANE-*`, `turnperf/`, and `turnperf2/`
evidence copies also remain uncommitted as required.

NO_SHIP
