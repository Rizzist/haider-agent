# v0.0.968 resume lane: full-family fix report

## Outcome and diagnosis

The four pre-gate failures are fixed, the requested full crate family is green,
and two independent verifiers returned `SHIP`.

The brief's route-attribution diagnosis identified a real product-law defect,
but it was not the direct cause of the three named never-opening failures. Those
fixtures never completed provider-open, so no connect error reached the route
classifier. The direct cause was `DaemonGraphFinalizationGuard` sleeping from
the provider safety-margin timeout until the absolute three-second client
deadline. That consumed the reserve intended to persist and deliver the
structured terminal. The repair therefore covers both failure classes:

1. A provider timeout before the caller deadline returns immediately to core,
   preserving the existing one-second terminalization reserve. A deadline fact
   is written only when the absolute caller deadline has actually elapsed.
2. `NetworkUnavailable` is negative-only. Provider classification, actor error
   attribution, stream state changes, retry telemetry, and the route-wait
   re-check all require an actual `RouteStatus::Unavailable` observation.
3. The replay ledger retains whether a tool boundary was crossed for the whole
   logical provider request. Usage is committed before, and separately from,
   terminal `Done` when that external boundary exists.

No file from another 968 lane's territory or the do-not-touch list changed.

## Citation audit

Every brief citation was found by construct rather than trusted by line number.

| Brief citation | Verdict | Current location |
|---|---|---|
| `crates/haider-provider/src/lib.rs:2150` | drifted | line 2150 is a rustls match arm; the route-gated classification is lines 2197-2205 |
| `run_budget_tests.rs:831` | drifted as a test location | the test starts at line 686; line 831 closes its wrong-terminal check; the reported deadline assertion is line 839 |
| `provider_deadline_rpc_tests.rs:161` | drifted | the test starts at line 95; line 161 is `trust_hooks`; the reported deadline assertion is line 169 |
| `actor_request_attempt_tests.rs:698` | correct | line 698 is the `usage`/`Done` batch exclusion assertion; the test starts at line 667 |

## Regression and mutation proof

The paired route tests are:

- `live_route_failure_retries_to_never_opening_provider_without_route_wait`
- `actually_down_route_waits_once_then_live_retry_never_opens_and_terminalizes`

Both use the same provider: the first open returns a connection-class failure
and the retry never opens. The first test reports a live route and forbids a
route-wait fact; the second reports a down route, requires exactly one route-wait
fact, restores the route, and requires the repeated failure not to park again.
Their outer bounds are derived in the test comments from the provider deadline
plus four 250 ms observation periods.

The requested polarity mutation was executed by inverting both negative route
checks. Under the mutation, the live-route test failed because it recorded
`WaitingForRoute`, while the down-route test failed because it issued request 2
before observing the required wait. The mutation run was `1 passed; 7 failed`
under the shared `route_` filter, including both named regressions. Restoring the
correct polarity made the same filter `8 passed; 0 failed`.

## Verification evidence

Every Cargo command used:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Daemon/client/CLI runs also used `HAIDER_TEST_SIBLINGS_PREBUILT=1` after both
binaries were prebuilt. `df -m /` was checked before every Cargo invocation and
never approached the 700 MiB stop threshold.

| Check | Result |
|---|---|
| `haider-rpc` full package | PASS |
| `haider-protocol` full package | PASS |
| `haider-store` full package | PASS |
| `haider-core` full package | PASS; runtime 68/68, prompt history 39 pass / 1 pre-existing ignore |
| `haider-tools` full package | PASS; unit 81 pass / 1 pre-existing ignore, integrations green |
| `haider-provider` full package | PASS; library 211/211, existing live tests remain ignored |
| `haider-platform` full package | PASS; library 33/33 and integrations green |
| `haider-accounts` full package | PASS; unit 10/10, integration 17 pass / 1 pre-existing keychain ignore |
| `haider-daemon --lib` | PASS; 860 passed / 3 pre-existing ignores |
| `haider-daemon --tests` | PASS; library 860/3 plus integration 103/103 and both one-test targets |
| `haider-tui` full package | PASS |
| `haider-client` full package | PASS; library 51/51 and integration 18/18 among all-green targets |
| `haider-cli` full package | PASS; all targets green |
| `haider-daemond` full package | PASS; all 17 targets green, including provider deadline RPC 1/1 in 2.35 s |
| original daemon never-opening pin | PASS |
| original daemond never-opening RPC pin | PASS |
| usage/Done exclusion plus opposite no-boundary batching pins | PASS; 8/8 filtered actor-attempt tests |
| route regression family after mutation restoration | PASS; 8/8 |
| resume reconnect/replay/restart/deadline/5xx family | PASS in full core/daemon family |
| affected all-target deny-warnings Clippy | PASS for provider, core, daemon, and daemond |
| test ledger | PASS; `test-baseline.txt` updated from 4248 to 4250 |
| locked metadata, rustfmt, diff check | PASS |
| built `haider` | Mach-O arm64, 102,538,736 bytes |
| built `haiderd` | Mach-O arm64, 182,470,448 bytes; exceeds 10 MiB |

The existing ignored tests were not added, weakened, or platform-gated by this
work.

## Independent verification

1. Provider/route verifier: `SHIP` after reading the final classification,
   actor gates and re-check, retry telemetry, deadline truth, paired mutation
   tests, and rerunning the focused provider/core/daemon/daemond pins.
2. Batching verifier: `SHIP` after checking both opposing batch pins, durable
   replay semantics across restart, full core runtime 68/68, all-target core
   Clippy, the test ledger, and diff hygiene.

## CI error registry walk

`checked: none` means the recorded class was read against this exact diff and no
instance was introduced or exposed. `fixed: file:line` identifies a class fixed
in this round. Class 93 is absent from the supplied persistent registry and is
not invented here.

| Class | Result | Evidence |
|---:|---|---|
| 1 | checked: none | No public struct/enum shape changed. |
| 2 | checked: none | No String/Vec/Option API rename or signature drift. |
| 3 | checked: none | Full family and Clippy found no ownership error. |
| 4 | checked: none | No private field was exposed or accessed cross-crate. |
| 5 | checked: none | No cfg-narrow import change. |
| 6 | checked: none | No duplicate import, method, or enum variant. |
| 7 | checked: none | No dependency edit; locked metadata resolves. |
| 8 | checked: none | Mutation was restored once and the final diff re-read. |
| 9 | checked: none | Affected all-target deny-warnings Clippy passes. |
| 10 | checked: none | No dead helper or unused value. |
| 11 | checked: none | No combinator/cast/return lint. |
| 12 | checked: none | No long-argument API added. |
| 13 | checked: none | No type-complexity diagnostic. |
| 14 | checked: none | No equality derive changed. |
| 15 | checked: none | No iterator-end rewrite. |
| 16 | checked: none | No range rewrite. |
| 17 | checked: none | The deadline fact mutex is not held across a new wait. |
| 18 | checked: none | No lint allowance or unsafe change. |
| 19 | checked: none | Touched Rust files pass rustfmt check. |
| 20 | fixed: test-baseline.txt:1 | Two regression tests move the ledger to 4250. |
| 21 | checked: none | Every test used the required 8 MiB stack. |
| 22 | checked: none | No tracing subscriber change. |
| 23 | checked: none | No migration/schema change. |
| 24 | checked: none | Provider catalog authority is unchanged. |
| 25 | checked: none | No performance claim or render benchmark. |
| 26 | checked: none | No filesystem/platform API change. |
| 27 | checked: none | No Windows wire or keepalive change. |
| 28 | checked: none | No process-tree runner change. |
| 29 | checked: none | No autospawn policy change. |
| 30 | checked: none | Terminal observers remain bounded and diagnostic. |
| 31 | checked: none | No Android/release artifact change. |
| 32 | checked: none | No release publish action. |
| 33 | checked: none | No runner behavior changed. |
| 34 | checked: none | No dependency module/feature added. |
| 35 | checked: none | No ambiguous trait call. |
| 36 | checked: none | No temporary borrowed through `?`. |
| 37 | checked: none | No cfg-boundary type changed. |
| 38 | checked: none | No collection key changed. |
| 39 | checked: none | Every touched test source compiled in its owning package. |
| 40 | checked: none | No cfg dependency-error conversion changed. |
| 41 | checked: none | No socket-path construction changed. |
| 42 | checked: none | No cold-binary launch timing assertion added. |
| 43 | checked: none | No descriptor close sweep. |
| 44 | checked: none | No socket-binding proof is claimed from sandbox execution. |
| 45 | checked: none | No cfg-Windows unsafe block. |
| 46 | checked: none | No runtime-root policy change. |
| 47 | checked: none | No filesystem walker change. |
| 48 | checked: none | New tests use existing declared test targets. |
| 49 | checked: none | No queued acknowledgement replay change. |
| 50 | checked: none | No exact serialized-size pin. |
| 51 | checked: none | No profile-lock change. |
| 52 | checked: none | No help viewport change. |
| 53 | checked: none | No runtime-root ownership change. |
| 54 | checked: none | Correct runner stack used and every later family ran. |
| 55 | checked: none | No cfg-Windows unit-valued binding. |
| 56 | checked: none | Terminal code remains reason-driven; no phase mapping changed. |
| 57 | checked: none | No UI layout pin. |
| 58 | checked: none | No CAS/inline threshold change. |
| 59 | checked: none | No roster suffix change. |
| 60 | checked: none | No IPC liveness change. |
| 61 | checked: none | Every new guarantee has a behavioral assertion. |
| 62 | checked: none | No public return type changed. |
| 63 | checked: none | No platform shell utility introduced. |
| 64 | checked: none | Both binaries are valid Mach-O; `haiderd` is 182,470,448 bytes. |
| 65 | checked: none | No raw errno enters an asserted outcome. |
| 66 | checked: none | No STT surface change. |
| 67 | checked: none | Sibling binaries were prebuilt and the flag exported. |
| 68 | checked: none | No swallowed error hardened. |
| 69 | checked: none | No executable discovery/path casing change. |
| 70 | checked: none | No workflow trigger or dispatch. |
| 71 | checked: none | Real daemon/client deadline RPC pin passes. |
| 72 | checked: none | Discovery environment matches the mandated family runner. |
| 73 | checked: none | No fixed-byte source scan. |
| 74 | checked: none | Real-daemon fixture home isolation is unchanged. |
| 75 | checked: none | No hub drain ownership change. |
| 76 | checked: none | No wire projection field change. |
| 77 | checked: none | Repository ledger and locked checks pass before handoff. |
| 78 | checked: none | No tag/release dispatch. |
| 79 | checked: none | No natural process-completion change. |
| 80 | checked: none | Daemond core-loop targets are green. |
| 81 | checked: none | No output-reader readiness change. |
| 82 | checked: none | No foreground/background ownership change. |
| 83 | checked: none | No completion-detach change. |
| 84 | checked: none | No serialized reconciliation timing change. |
| 85 | checked: none | No late-cancellation classification change. |
| 86 | checked: none | No exit-observer error change. |
| 87 | checked: none | No thread-count lifecycle fence. |
| 88 | checked: none | No manifest replacement change. |
| 89 | checked: none | No endpoint-scope change. |
| 90 | checked: none | No sparse-file fixture change. |
| 91 | checked: none | No line-ending-sensitive source boundary. |
| 92 | checked: none | No paused-time reconciliation fence change. |
| 94 | checked: none | New outer bounds state provider budget plus observation allowance. |
| 95 | checked: none | No external-state wait was added on an open negotiated transport. |
| 96 | fixed: crates/haider-daemon/src/worker.rs:747 | A provider safety-margin timeout must preserve, not sleep through, the terminal-delivery reserve. |
| 97 | fixed: crates/haider-provider/src/lib.rs:2197 | Route-outage attribution is negative-only at transport classification, actor entry, and waiting re-check. |
| 98 | fixed: crates/haider-core/src/actor.rs:4651 | Usage is independently durable before `Done` after an external tool boundary. |

### Section D additions from this round

- **#96 provider timeout consumes its terminal-delivery reserve** — an adapter
  times out at `caller deadline - safety margin`, then a finalization guard
  sleeps until the caller deadline, making an early structured terminal
  impossible. Preserve the reserve; do not persist a deadline fact before the
  absolute deadline actually elapses.
- **#97 route attribution without a negative route observation** — a refused,
  reset, or interrupted provider on an available/unknown route is a provider
  transport failure, never evidence of route loss. Require
  `RouteStatus::Unavailable` at classification, actor entry, and every waiting
  re-check.
- **#98 a replay optimization erases an external durability boundary** — an
  empty final tool accumulator does not prove no tool was dispatched. Retain
  the boundary in the durable replay prefix and forbid coalescing usage with
  terminal `Done` across it.
