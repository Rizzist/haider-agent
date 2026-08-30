# CI preparation and registry audit

## Gate results

The repository guard passes with production unsafe count 188 and test unsafe
count 15. The initial fail-without-fix characterization preceded that guard by
mistake; every final verification phase restored the CI order. `git diff
--check`, conflict-marker inspection, and Rust 2024 formatting of every touched
Rust file passed. There are no
manifest, lockfile, workflow, contract-fixture, version, xtask, or baseline
changes.

The release-gate environment was used for every Cargo command; real-daemon
fixtures and the final gates used temporary machine-user `HOME`/`USERPROFILE`
roots and never set a nested `CARGO_HOME`. `df -m /` ran immediately before each
Cargo command and never fell below the 700 MiB stop threshold; this continuation
had roughly 40-56 GiB free. `cargo metadata --locked` passed, and `cargo tree -d
--locked` was reviewed; all duplicate major/version families predate this lane.

Verification results:

- New end-to-end binary: the normal-completion descendant test passes in the
  full binary and five additional direct repetitions; 9 tests pass and the
  untouched class #80 child-settlement test remains red.
- `haider-tools`: 76 library tests passed / 1 ignored, all 8 background-task
  tests passed, all 27 process tests passed, and every other integration/doc
  test passed.
- `haider-daemon --lib`: 828 passed, 3 ignored; the brief's 826 pin had drifted.
- `haider-daemond`: the required unfiltered suite fails only class #80. With
  that one known red test filtered, every binary passes;
  `ephemeral_liveness_tests` is 6/6 and `lifecycle_tests` is 37/37.
- `haider-rpc`: all tests pass and the wire-golden frame count remains 180.
- Prebuilt `haiderd` and `haider` are valid arm64 Mach-O files of 172 MiB and
  96 MiB respectively; `haiderd` exceeds the 10 MiB guard.
- The final post-repair workspace `cargo check --all-targets --locked` passes.
- The final post-repair `cargo clippy --workspace --all-targets --locked -- -D
  warnings` passes.

The registry's referenced `$T/ci-prep.sh` is not present in this worktree and no
`$T` value was supplied, so its applicable checks were run directly. A
workspace-wide test command was not substituted because the lane brief
explicitly forbids `cargo test --workspace` and supplies the required family
list instead.

## Verify-until-SHIP

1. Local verification iteration 1: `FIX`. The full tools suite exposed a
   normal-exit race that lost an immediate four-byte output chunk; reader-start
   acknowledgements and ready-read bias fixed that reproduction.
2. Local verification iteration 2: `FIX`. The real-RPC outliving-child case
   exposed the remaining same-scheduler-turn readiness race, losing `leader`.
   A scheduler yield before stop—without a timer—fixed it; the exact test then
   passed once through Cargo and five more direct repetitions.
3. Requested verifier iteration 1: `FIX`. It identified that the first
   normal-completion implementation could still sweep when Windows ownership
   detachment failed.
4. Requested verifier iteration 2: `FIX`. Excluding the unrelated class #80
   failure did not change that process-contract finding.
5. Focused verifier diagnostics isolated the issue: the ordinary detachment,
   cancellation/timeout/`task_kill`, and model-manual clauses each returned
   `SHIP`; only the normal-detach-error fallback returned `FIX`.
6. The fallback now abandons the exact fail-closed Windows Job authority and
   reports the fault, rather than invoking teardown.
7. Requested final combined verifier iteration: `SHIP`. It reviewed the
   post-repair, measured diff after workspace check and deny-warnings Clippy and
   answered the exact three-clause contract question with `SHIP`.

These verdicts assess the requested process contract. They do not override the
release `NO_SHIP` caused by the one untouched production failure the suite
exposes.

## §A–§D class audit

`checked` means the class was inspected and has no lane-introduced violation;
`gap` is intentionally not represented as a pass.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed | Added `NormalCompletionDetached` additively and checked every lifecycle match/constructor workspace-wide. |
| 2 | fixed | Exported `detach_process_group` and extended internal reader arguments; every call site was reconciled. |
| 3 | checked | Workspace check found no ownership errors. |
| 4 | checked | Tests use public RPC/fixture APIs only. |
| 5 | checked | Unix/Windows-only imports and commands are cfg-scoped in the new test. |
| 6 | checked | No production table/import/variant changed. |
| 7 | checked | No manifest/lock change; locked metadata/check/clippy pass. |
| 8 | checked | The supervisor change was applied once, re-read, formatted, and compiled workspace-wide. |
| 9 | checked | Final workspace clippy passes. |
| 10 | checked | No dead or unused helper remains. |
| 11 | fixed | Removed `useless_conversion` and replaced manual membership search in `core_loop_e2e_tests.rs:118,1221`. |
| 12 | checked | No new production argument list. |
| 13 | checked | No clippy type-complexity finding. |
| 14 | checked | New comparable types satisfy existing derives; no new public data type. |
| 15 | checked | No `.last()` sweep or affected iterator. |
| 16 | checked | No manual-range diagnostic. |
| 17 | fixed | The process-control gate covers only the liveness/signal seam and is dropped before stdin awaits. |
| 18 | checked | Test-only `expect` allow is singular; production remains unchanged. |
| 19 | checked | `rustfmt --edition 2024 --check` passes on every touched Rust file. |
| 20 | checked | Existing tests were deliberately updated; no test-count baseline was changed. |
| 21 | checked | Every test command used `RUST_MIN_STACK=8388608`. |
| 22 | checked | The fixture does not install a tracing subscriber. |
| 23 | checked | No migration or schema change. |
| 24 | checked | Cross-provider fixture routing does not alter production catalog authority. |
| 25 | checked | No render measurement. |
| 26 | checked | Platform-specific process fixtures are cfg-separated; no directory-fsync assertion. |
| 27 | checked | No Windows wire implementation change. |
| 28 | checked | Cancellation uses one bounded observer; Windows execution remains a CI-kernel gap. |
| 29 | checked | The suite connects to an already-started daemon; no autospawn policy change. |
| 30 | fixed | The known infinite-settlement reproduction runs in an isolated test process with an 8 s kill boundary. |
| 31 | checked | No Android change. |
| 32 | checked | No release action. |
| 33 | checked | Test-runner behavior is local to the new binary. |
| 34 | checked | No dependency module or Cargo feature introduced. |
| 35 | checked | No ambiguous trait-method call. |
| 36 | checked | No temporary reference is borrowed through `?`. |
| 37 | checked | Unix and Windows detach arms return the same typed `io::Result<()>`; workspace check passes. |
| 38 | checked | Fleet assertions compare projected strings with strings. |
| 39 | checked | Every changed existing test file compiles under package and workspace all-target checks. |
| 40 | checked | No cfg-windows dependency error conversion added. |
| 41 | checked | Shared real-daemon support owns the platform endpoint; local macOS suite connects successfully. |
| 42 | checked | No launch latency assertion; all waits are behavioral deadlines. |
| 43 | checked | Real process spawning is exercised, but no descriptor-sweep implementation changed. |
| 44 | gap | Local macOS IPC passed inside this lane; unsandboxed and other-kernel reruns belong to orchestration/CI. |
| 45 | fixed | The Windows Job limit call was refactored under the smallest allowed function with its SAFETY comment; unsafe count stayed 188/15. |
| 46 | checked | Shared daemon fixture creates and validates its own temporary runtime root. |
| 47 | fixed | A real ignored `target/` containing a 1 TiB sparse artifact is exercised below the provider boundary. |
| 48 | checked | This is a Cargo integration test, not a daemon source sibling module. |
| 49 | checked | No acknowledgement/pipeline logic change. |
| 50 | fixed | Platform-specific tool-prose byte pins were reconciled and the platform-neutral reduction invariant remains. |
| 51 | checked | No profile-lock change. |
| 52 | checked | No TUI layout change. |
| 53 | checked | No runtime-root permission implementation change. |
| 54 | checked | Required 8 MiB test stack was present; every required earlier binary reached and passed. |
| 55 | checked | No cfg-windows unit-valued binding. |
| 56 | checked | No deadline-to-exit-code mapping change; test deadlines fail rather than count as behavior success. |
| 57 | checked | No UI pin/layout change. |
| 58 | checked | No result-storage threshold change. |
| 59 | checked | No roster rendering change. |
| 60 | gap | Windows process/connection semantics require the Windows CI kernel. |
| 61 | checked | Every claimed behavior has an assertion; no unasserted benchmark guarantee. |
| 62 | checked | The public process API addition did not change an existing return type; workspace call sites compile. |
| 63 | checked | No platform archive tool. |
| 64 | fixed | ENOSPC-truncated outputs were identified with `file`, removed exactly, rebuilt, and `haiderd` is now a valid 172 MiB Mach-O. |
| 65 | checked | Assertions use typed semantic outcomes, not raw errnos. |
| 66 | checked | No STT surface. |
| 67 | checked | `haiderd` and `haider` were prebuilt and every Cargo suite used the sibling-prebuilt flag. |
| 68 | checked | No swallowed error was hardened. |
| 69 | checked | No Windows executable discovery. |
| 70 | checked | No workflow trigger or dispatch. |
| 71 | fixed | New tests drive a real daemon, IPC, tool execution, journal, projection, and client-visible terminal result. |
| 72 | gap | The owner-mandated environment disables discovery; this suite does not claim the native credential path. |
| 73 | checked | No fixed-window source scan. |
| 74 | fixed | Every daemon/Cargo process uses one temporary machine-user home, not the developer home. |
| 75 | checked | No actor shutdown implementation change. |
| 76 | checked | Fork provenance is asserted through the RPC projection and child replay. |
| 77 | fixed | The initial characterization-order miss was corrected; the guard ran first in final CI-prep and exited 0. |
| 78 | checked | No release/tag dispatch. |

## §D addition from lane 967-A2

- **#79 process completion terminates an outliving background descendant** —
  corrected by the lane 967-P1 owner decision. Natural leader completion now
  closes foreground capture without a group sweep; the descendant is unmanaged
  and can write its durable marker, while late descendant bytes are deliberately
  absent from the `leader` foreground result. The updated
  `process_exec_normal_completion_leaves_outliving_descendant_alone` pin covers
  the real RPC path. Durable long-running output belongs to `background=true`.

- **#80 terminal run completion coupled to aggregate session Idle** — a child run
  can reach `Done` while another queued child run makes the session active.
  `mirror_until_child_terminal` nevertheless requires a later aggregate `Idle`
  fence before releasing the already-terminal report, so the parent waits
  forever. Check: queue a second child turn through real RPC while run one is
  active; require run one `Done` and parent continuation. The new
  `terminal_child_run_without_session_idle_still_releases_parent` test fails in
  a bounded isolated process. Production repair belongs to the concurrent
  child-lifecycle lane; this lane does not edit `delegation.rs`.

- **#81 normal-exit output stop outruns reader readiness** — closing foreground
  capture as soon as the leader exits can stop a newly spawned output-reader
  task before it polls the pipe, or can observe leader exit in the same
  scheduler turn before the reactor schedules an already-ready pipe. Startup
  acknowledgements and ready-read bias close the first window; a timer-free
  scheduler yield before stop closes the second. Check the exact-byte streaming
  pin and repeat the real-RPC outliving-child test to expose scheduler-sensitive
  loss.

- **#82 foreground/background ownership split-brain** — changing only
  foreground `process_exec` leaves the durable background supervisor's old
  natural-exit sweep in place, contradicting an unqualified model manual and
  owner rule. Whenever process ownership semantics change, grep every call to
  the shared group-termination ladder and reconcile foreground, background,
  `task_kill`, recovery, and shutdown separately. The background natural-exit
  survival pin and the existing live `task_kill` group-kill pin settle both
  sides of this contract.

- **#83 normal-completion detach failure falls back into teardown** — a normal
  leader exit must remain a non-sweep outcome even when platform ownership
  detachment fails. On Windows, merely closing a Job with
  `KILL_ON_JOB_CLOSE` is itself a sweep, so a generic cleanup/release fallback
  violates the product decision. Report the typed fault, remove the exact token
  and PID lookup coordinates, and deliberately abandon that handle until OS
  process exit; never silently enter TERM/grace/KILL. The verifier's first two
  combined iterations found this class, and its focused clauses settled the
  repair boundary.
