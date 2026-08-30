# CI-PREP — lane 967-gc1

## Direct gate result

The brief's `$T/ci-prep.sh` is not present in the worktree, no `T` environment
variable is defined, and no replacement script was found under `scripts/`.
The applicable checks were therefore run directly in CI order:

- `bash scripts/check-unsafe-counts.sh`: PASS (`production=188`, `test=15`)
- `cargo metadata --locked`: PASS
- Rust 2024 formatting check on every changed `.rs`: PASS
- `git diff --check`, unmerged/conflict-marker scan: PASS
- manifest/lock/workflow/contract-fixture parse: not applicable; none changed
- `cargo tree -d --locked`: reviewed; no dependency or lockfile change
- `cargo check --workspace --all-targets --locked`: PASS
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: PASS
- mandated package suites: PASS
- final binaries: arm64 Mach-O; `haiderd=178,514,048` bytes and
  `haider=100,388,000` bytes

No version, xtask, test-count baseline, Cargo manifest, lockfile, workflow, or
contract fixture was changed. Two new reusable local-gate classes were found
and are appended below for the integration registry.

## §A–§D registry audit

| Class | Result |
|---:|---|
| 1 | checked — every constructor/match for the new mixed commit enums compiles workspace-wide. |
| 2 | checked — changed mixed-commit signatures and every caller compile. |
| 3 | checked — workspace check found no moved-after-use error. |
| 4 | checked — tests use public store behavior; no private field was exposed. |
| 5 | checked — Apple-only operations/imports remain cfg-scoped; all-target clippy is clean. |
| 6 | checked — new enum variants are exhaustively matched and imports are singular. |
| 7 | checked — no Cargo manifest/lock edit; locked metadata/check/tests pass. |
| 8 | checked — no mechanical sweep or non-idempotent rewrite. |
| 9 | checked — deny-warnings clippy found no collapsible control flow. |
| 10 | checked — no dead/unused helper remains. |
| 11 | checked — deny-warnings clippy found no combinator/borrow/cast/default family issue. |
| 12 | checked — no over-wide function signature finding. |
| 13 | checked — no type-complexity finding. |
| 14 | checked — no incompatible Eq/PartialEq derive. |
| 15 | checked — no iterator-last rewrite. |
| 16 | checked — no manual-range finding. |
| 17 | checked — no lock guard crosses await; admission atomics and store mutex boundaries are separate. |
| 18 | checked — no duplicate lint attribute; production contains no new unwrap/expect. |
| 19 | checked — Rust 2024 format check passes on all 15 changed Rust files. |
| 20 | checked — store tests changed; baseline intentionally not bumped per lane law. |
| 21 | checked — every mandated suite used the 8 MiB stack environment. |
| 22 | checked — no tracing subscriber installation. |
| 23 | checked — no schema/migration change. |
| 24 | checked — provider catalog authority is untouched. |
| 25 | checked — turn wall comes from the real RPC harness; fence wall from the explicit filesystem harness, reported separately. |
| 26 | checked — directory sync assertions are policy-selection assertions; Windows remains no-op for directory sync. |
| 27 | checked — Windows wire behavior untouched. |
| 28 | checked — process test runner behavior untouched. |
| 29 | checked — autospawn behavior untouched. |
| 30 | fixed — prompt-fork fixture now waits on durable Session Idle instead of racing the post-Run-Done append. |
| 31 | checked — Android untouched. |
| 32 | checked — no release rerun/action. |
| 33 | checked — no test-runner behavior change remains after removing the temporary measurement harness. |
| 34 | checked — no dependency module or Cargo feature introduced. |
| 35 | checked — no trait-method ambiguity. |
| 36 | checked — no temporary borrowed through `?`. |
| 37 | checked — cfg type seams unchanged; all-target workspace check passes. |
| 38 | checked — no collection key type changed. |
| 39 | checked — changed sibling test file compiles in its owning crate. |
| 40 | checked — no Windows dependency error conversion. |
| 41 | checked — no endpoint/path change; real daemond UDS suites pass. |
| 42 | checked — no launch timing assertion. |
| 43 | checked — no descriptor sweep change. |
| 44 | checked locally — real UDS suites pass in this managed lane; cross-kernel confirmation remains CI-owned. |
| 45 | checked — unsafe-count guard passes and no unsafe block was added. |
| 46 | checked — runtime-root derivation untouched. |
| 47 | checked — no walker change. |
| 48 | checked — test remains the declared sibling `group_commit_tests.rs` module. |
| 49 | checked — acknowledgement delete is idempotent; mixed-group test proves one outer commit and no pending row. |
| 50 | checked — no platform-dependent byte pin. |
| 51 | checked — profile-lock code untouched. |
| 52 | checked — TUI help untouched. |
| 53 | checked — runtime-root permissions untouched. |
| 54 | checked — CI runner stack env used; every required binary reached completion. |
| 55 | checked — no cfg-windows unit binding. |
| 56 | checked — deadline reason/exit mapping untouched. |
| 57 | checked — no UI layout pin. |
| 58 | checked — CAS inline threshold unchanged. |
| 59 | checked — roster grammar untouched. |
| 60 | checked — Windows liveness untouched. |
| 61 | fixed — provider-view Barrier and generic-CAS Full contracts now have separate policy tests; report explicitly records the negative whole-turn result. |
| 62 | checked — no existing public return type changed; new mixed-commit types are additive. |
| 63 | checked — no external archive tool path. |
| 64 | checked — final `haiderd` is valid Mach-O and exceeds 10 MB. |
| 65 | checked — no raw errno enters a public outcome. |
| 66 | checked — STT untouched. |
| 67 | checked — `haiderd`/`haider` prebuilt and sibling flag used. |
| 68 | checked — no swallowed error hardened. |
| 69 | checked — no executable path construction. |
| 70 | checked — workflow triggers untouched. |
| 71 | checked — real prebuilt `status --json` discovery smoke passes. |
| 72 | checked — the smoke test explicitly exercises enabled discovery; other mandated suites use the hermetic disable. |
| 73 | checked — no fixed-window source scan. |
| 74 | checked — machine-user-global state and subprocess fixtures untouched. |
| 75 | checked — hub shutdown cancellation ownership untouched; full daemond lifecycle suite passes. |
| 76 | checked — no wire projection field. |
| 77 | checked — unsafe guard ran first and passed. |
| 78 | checked — no tag or release dispatch. |

## §D additions — 967-gc1 local verification

- **#79 shared durability helper weakens callers outside the intended scope**
  — changing the generic CAS batch finisher from Full to Barrier also changed
  checkpoint preimages, whose public contract requires persistence at return.
  Keep contract-specific finishers separate and grep every caller of a shared
  durability helper before downgrading it. Fixed by retaining Full for
  `Cas::put_batch`, adding a provider-view-only ordered finisher, and pinning
  both policies independently.
- **#80 run terminality is not aggregate-session settlement** — Run Done is
  journaled before the separate Session Idle fact. A snapshot taken at Done
  and compared after another operation can mistake the legitimate Idle append
  for mutation by that operation. Fixtures requiring a stable session snapshot
  must wait on the durable Idle event, never a sleep. Fixed in
  `core_loop_e2e_tests.rs` and re-proved by the full daemond suite (core loop
  10/10).
- **#81 a higher group count is not a throughput result** — a scheduler-yield
  window made one or two mixed groups in every four-turn cohort but left the
  median WAL commit count unchanged and reduced measured throughput by 5.6%.
  Any intentional group-commit wait must report end-to-end latency, throughput,
  and total physical commit markers; batching counters alone can reward work
  reshuffling that does not amortize a transaction. Fixed by rejecting the
  yield window and shipping no intentional delay.
