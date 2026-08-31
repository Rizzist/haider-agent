# CI-PREP — lane 968-wfcont

## Result

The repository does not contain the brief's `$T/ci-prep.sh` and `T` is unset,
so the applicable lane-scoped checks were run directly. The brief explicitly
forbids workspace-wide Clippy; the six affected packages were checked together
with `--all-targets --locked -- -D warnings` instead.

- unsafe-count guard: PASS (`production=188`, `test=15`)
- locked metadata: PASS
- dependency duplicates: reviewed; this lane changes no manifest or lockfile
- Rust formatting, `git diff --check`, conflict-marker and unmerged-index
  checks: PASS
- contract JSON and GitHub workflow YAML parsing: PASS; no such file changed
- affected-package deny-warning Clippy: PASS (`haider-protocol`, `haider-store`,
  `haider-core`, `haider-daemon`, `haider-daemond`, `haider-tui`)
- complete affected suites: protocol PASS; store PASS; core PASS; daemon lib
  PASS (859 passed, 3 pre-existing live-test ignores); real-RPC core loop PASS
  (14/14)
- test ledger: updated at ship from 4082 to 4239
- final binaries: arm64 Mach-O; `haiderd=180,849,968` bytes and
  `haider=102,348,944` bytes; the daemon exceeds 10 MiB

## Citation audit

All three locations in the brief are **wrong**, including on cited commit
`d78bd15`, rather than merely drifted:

- `worker.rs:12082` is the `SshList` enum arm; the current daemon dispatcher
  refresh is at `crates/haider-daemon/src/worker.rs:13828`.
- `actor.rs:1147` is `ContextCompactionOutcome`; the current logical-request
  refresh is at `crates/haider-core/src/actor.rs:2759`.
- `event_store.rs:4801` begins active-graph reduction; the current recurrence
  decision is at `crates/haider-store/src/event_store.rs:4825`.

## Continuation and mutation evidence

The invariant is: a changed `(run_id, state_digest)` is durable graph progress
and may spend another autonomous continuation, subject to the run deadline,
max-cost enforcement, and `max_provider_requests_per_turn`. Repeating the same
pair proves no progress and remains fail-closed. Recovery additionally requires
the latest provider-attempt marker to precede the deferral; a later marker is
delivery-ambiguous after a crash and remains on generic fail-closed recovery.

The three-stage test first reproduced the one-refresh mutation. With the actor
condition temporarily changed from `provider_attempt == 0` to
`provider_attempt == 0 && provider_request_count == 0`, it failed with:

```text
Errored; failure=Some((WorkflowUnfinished,
"workflow_unfinished: graph ... repeated the same unfinished workflow state
after an autonomous continuation ...")); observed_events=49
```

The mutation was reverted immediately. The restored three-stage and five-stage
tests reach `Done`; the request-cap test uses `2 < 3` stages and returns typed
`LoopLimit`; and the crash checkpoint after IMPLEMENT deferral and before the
VERIFY provider attempt resumes from the journal and reaches `Done`.

## §A–§D registry walk

`checked: none` means the class was inspected and this lane introduces no
instance. `fixed` names the lane location that reconciles a class it exercises.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed: `crates/haider-protocol/src/graph.rs:2183` | The additive deferral field and every in-tree constructor compile. |
| 2 | fixed: `crates/haider-core/src/actor.rs:1067` | Guard/recovery signature changes and all callers compile. |
| 3 | checked: none | No moved-after-use diagnostic. |
| 4 | checked: none | No private field was exposed to integration tests. |
| 5 | checked: none | No cfg-narrow import was added. |
| 6 | fixed: `crates/haider-daemon/src/runtime.rs:1154` | The new recovered-work variant is exhaustively routed. |
| 7 | checked: none | No manifest/lock edit; locked metadata passes. |
| 8 | checked: none | No mechanical sweep was used. |
| 9 | checked: none | Targeted deny-warning Clippy is clean. |
| 10 | checked: none | No dead or unused production helper remains. |
| 11 | checked: none | Targeted deny-warning Clippy is clean. |
| 12 | checked: none | No over-wide production signature finding. |
| 13 | checked: none | No type-complexity finding. |
| 14 | checked: none | No derive seam changed incompatibly. |
| 15 | checked: none | No iterator-last rewrite. |
| 16 | checked: none | No manual-range finding. |
| 17 | checked: none | No guard is held across an await. |
| 18 | checked: none | No duplicate lint attribute or production unwrap/expect. |
| 19 | checked: none | `cargo fmt --all` and diff check pass. |
| 20 | fixed: `test-baseline.txt:1` | The ledger was updated after adding the four e2e tests. |
| 21 | checked: none | Every test used the mandated 8 MiB stack. |
| 22 | checked: none | No tracing subscriber installation. |
| 23 | checked: none | No schema or migration change. |
| 24 | checked: none | Provider catalog authority is untouched. |
| 25 | checked: none | No rendering benchmark. |
| 26 | checked: none | No filesystem platform seam changed. |
| 27 | checked: none | Windows wire semantics are untouched. |
| 28 | checked: none | No process-test runner change. |
| 29 | checked: none | Autospawn behavior is untouched. |
| 30 | fixed: `crates/haider-daemond/tests/core_loop_e2e_tests.rs:1392` | Scripts contain exactly two terminal segments per stage and waits fail on terminal error. |
| 31 | checked: none | Android is untouched. |
| 32 | checked: none | No release action. |
| 33 | checked: none | No runner behavior change. |
| 34 | checked: none | No dependency feature/module introduced. |
| 35 | checked: none | No trait-method ambiguity. |
| 36 | checked: none | No temporary borrowed through `?`. |
| 37 | checked: none | No cfg-boundary type change. |
| 38 | checked: none | No collection key mismatch. |
| 39 | fixed: `crates/haider-daemond/tests/core_loop_e2e_tests.rs:1392` | Changed tests compile under all-target Clippy and their complete binary passes. |
| 40 | checked: none | No Windows dependency error conversion. |
| 41 | checked: none | Real UDS daemon tests use bounded short endpoint paths and pass locally. |
| 42 | checked: none | No cold-launch timing assertion. |
| 43 | checked: none | No descriptor sweep change. |
| 44 | checked: none | Local real-UDS proof ran in the managed lane; other kernels remain CI-owned. |
| 45 | checked: none | Unsafe-count guard passes; no unsafe block added. |
| 46 | checked: none | Runtime-root derivation is untouched. |
| 47 | checked: none | No filesystem walker change. |
| 48 | checked: none | New tests are in an existing Cargo integration-test binary. |
| 49 | checked: none | No queued acknowledgement path change. |
| 50 | checked: none | No platform-dependent byte pin. |
| 51 | checked: none | Profile-lock code is untouched. |
| 52 | checked: none | TUI layout is untouched. |
| 53 | checked: none | Runtime-root permissions are untouched. |
| 54 | checked: none | Correct runner environment used; all required binaries reached completion. |
| 55 | checked: none | No cfg-Windows unit binding. |
| 56 | checked: none | No deadline reason/exit mapping change. |
| 57 | checked: none | No UI layout pin changed. |
| 58 | checked: none | CAS inline threshold is untouched. |
| 59 | checked: none | Roster grammar is untouched. |
| 60 | checked: none | Windows connection liveness is untouched. |
| 61 | fixed: `crates/haider-daemon/src/g1_todo_runtime_tests.rs:426` | The updated contract is asserted as BUILD→VERIFY rebinding, not merely documented. |
| 62 | checked: none | Existing public return types are unchanged. |
| 63 | checked: none | No external archive tool. |
| 64 | checked: none | Both rebuilt binaries are valid Mach-O; `haiderd` exceeds 10 MiB. |
| 65 | checked: none | No raw errno reaches a typed outcome. |
| 66 | checked: none | STT is untouched. |
| 67 | checked: none | Both siblings were rebuilt; daemon tests used the prebuilt flag. |
| 68 | checked: none | No swallowed error was hardened. |
| 69 | checked: none | No executable-path construction. |
| 70 | checked: none | No CI trigger or dispatch change. |
| 71 | checked: none | This lane's required proof is real-daemon RPC; release smoke behavior is untouched. |
| 72 | checked: none | Credential discovery is untouched. |
| 73 | checked: none | No fixed-window source pin. |
| 74 | checked: none | No machine-user-global subsystem added. |
| 75 | checked: none | Hub shutdown ownership is untouched. |
| 76 | checked: none | No CLI wire projection field added. |
| 77 | checked: none | Repository unsafe guard passes; no repository guard was modified. |
| 78 | checked: none | No tag/release dispatch. |
| 79 | checked: none | Process completion ownership is untouched. |
| 80 | checked: none | E2E terminal waits do not conflate run terminality with session idle. |
| 81 | checked: none | Process output-reader behavior is untouched. |
| 82 | checked: none | Foreground/background process ownership is untouched. |
| 83 | checked: none | Detach-failure fallback is untouched. |
| 84 | checked: none | No paused-time real-process test. |
| 85 | checked: none | No process terminal-classification change. |
| 86 | checked: none | No process exit-observer change. |
| 87 | checked: none | No thread-count lifecycle fence. |
| 88 | checked: none | No staging-file publication change. |
| 89 | checked: none | No Windows endpoint parent assertion. |
| 90 | checked: none | No sparse-file fixture. |
| 91 | checked: none | No line-ending-sensitive source pin. |
| 92 | checked: none | No maintenance-loop counter/timer. |
| 93 | checked: none | No subprocess sampling throughput assertion. |
| 94 | checked: none | No new deadline; existing e2e deadlines enclose only their bounded RPC waits. |
| 95 | checked: none | No long external-state wait is left on an open negotiated connection. |

No new CI error class was discovered by this lane.
