# CI preparation and registry audit

## Gate results

The repository guard passes with production unsafe count 188 and test unsafe
count 15. `git diff --check`, conflict-marker inspection, and Rust 2024
formatting of every touched Rust file pass. P1 has no manifest, lockfile,
workflow, contract-fixture, version, xtask, or baseline change. JSON fixtures
changed by the integrated base and all workflow YAML files parse successfully.

The release-gate environment was used for every Cargo command; real-daemon
fixtures use temporary machine-user `HOME`/`USERPROFILE` roots and never set a
nested `CARGO_HOME`. `df -m /` preflights remained above 12 GiB free. `cargo
metadata --locked` passed, and `cargo tree -d --locked` was reviewed; P1 changes
no dependency edge.

Verification results:

- `haider-tools`: 215 passed / 1 ignored across the complete crate; its focused
  background-task and process binaries are 8/8 and 28/28.
- `haider-daemon --lib`: 833 passed / 3 ignored. W1 added five tests after the
  brief's 828/3 correction was written.
- `haider-daemond`: every binary passes (134 tests total), including the
  unfiltered `core_loop_e2e_tests` at 10/10.
- `haider-rpc`: 154 tests pass. The current wire fixture contains 182 frames:
  180 is the frozen v0.0.966 prefix and K1 appends two frames.
- `haider-cli --test status_discovery_smoke_tests`: 1/1 passes.
- Prebuilt `haiderd` and `haider` are valid arm64 Mach-O files of 178,971,504
  and 100,419,568 bytes respectively; `haiderd` exceeds the 10 MiB guard.
- The workspace all-target `cargo check` and deny-warnings Clippy commands pass.

The registry's referenced `$T/ci-prep.sh` is not present in this worktree and no
`$T` value was supplied, so its applicable checks were run directly. A
workspace-wide test command was not substituted because the lane brief
explicitly forbids `cargo test --workspace` and supplies the required family
list instead.

## Verify-until-SHIP

1. Integration verification iteration 1: `FIX`. The rebased real-RPC test name
   and comment described P1, but its body still asserted the old sweep contract;
   the body was corrected. A direct broker-teardown descendant test was also
   missing and was added.
2. Integration verification iteration 2: `FIX`. Focused process tests exposed
   paused-Tokio-time races with real child/kernel progress. The affected natural
   completion and cancellation tests moved to real time, and Darwin's
   zombie-only `EPERM` interpretation gained synchronous waitability proof.
3. Integration verification iteration 3: `FIX`. The complete crate run found
   two remaining paused cancellation/grace tests could auto-fire the unrelated
   60-second wall bound and start a second sweep. Both now use bounded real time;
   the two focused cases then passed 20/20 each and the full crate passed.
4. Independent verifier iteration 1: `FIX`. It found that cancellation during
   post-exit CAS ingestion could retroactively label an already-detached command
   `Cancelled`, even though an unmanaged descendant could survive. Natural
   leader completion now wins the classification boundary; a late cancel cannot
   mask successful completion or a real CAS failure. A descendant + gated-CAS
   regression pins the invariant.
5. Independent verifier iteration 2: `FIX`. An injected exit-observer failure
   could enter the normal-completion detach path while its leader and
   descendants were still live. Observer failure now starts the same supervised
   teardown ladder as other supervision faults, and a direct injected-failure
   test proves the owned group is gone.
6. CI-prep iteration: `FIX`. The final deny-warnings Clippy pass found the new
   test's `expect` calls lacked the repository's test-only allowance. The
   allowance is scoped to that one `#[cfg(test)]` function; the repeated
   workspace Clippy pass is clean.
7. Independent verifier iteration 3: `FIX`. It found the same exit-observer
   error bug in the background supervisor: the error path could await a live
   leader indefinitely and then detach because the pre-P1 unconditional sweep
   had masked that branch. Background observer failure now enters teardown,
   with its own injected-failure, TERM-ignoring descendant regression.
8. Independent verifier iteration 4: `SHIP` after re-auditing the exact final
   foreground/background supervision tree and the complete rerun evidence.

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
| 18 | fixed | Observer-failure `expect` allowances are scoped to the foreground test function and background `#[cfg(test)]` module; production remains unchanged. |
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
| 33 | fixed | Two real-process tests no longer pause Tokio time after E1 moved exit observation to an external kernel-notification thread. |
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
| 44 | checked | Local macOS IPC is available in this sandbox and every required daemon binary passed; other kernels remain CI coverage. |
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
| 60 | checked | No connection-liveness seam changed; the Windows Job ownership changes were audited separately from IPC connection retirement. |
| 61 | checked | Every claimed behavior has an assertion; no unasserted benchmark guarantee. |
| 62 | checked | The public process API addition did not change an existing return type; workspace call sites compile. |
| 63 | checked | No platform archive tool. |
| 64 | checked | `file` identifies both prebuilt siblings as arm64 Mach-O; `haiderd` is 178,971,504 bytes. |
| 65 | checked | Assertions use typed semantic outcomes, not raw errnos. |
| 66 | checked | No STT surface. |
| 67 | checked | `haiderd` and `haider` were prebuilt and every Cargo suite used the sibling-prebuilt flag. |
| 68 | checked | No swallowed error was hardened. |
| 69 | checked | No Windows executable discovery. |
| 70 | checked | No workflow trigger or dispatch. |
| 71 | fixed | New tests drive a real daemon, IPC, tool execution, journal, projection, and client-visible terminal result. |
| 72 | checked | The owner-mandated environment disables discovery and P1 does not touch credential discovery; no claim is made about that separate path. |
| 73 | checked | No fixed-window source scan. |
| 74 | fixed | Every daemon/Cargo process uses one temporary machine-user home, not the developer home. |
| 75 | checked | No actor shutdown implementation change. |
| 76 | checked | Fork provenance is asserted through the RPC projection and child replay. |
| 77 | fixed | The initial characterization-order miss was corrected; the guard ran first in final CI-prep and exited 0. |
| 78 | checked | No release/tag dispatch. |

## §D additions during 967 integration

- **#79 process completion terminates an outliving background descendant** —
  corrected by the lane 967-P1 owner decision. Natural leader completion now
  closes foreground capture without a group sweep; the descendant is unmanaged
  and can write its durable marker, while late descendant bytes are deliberately
  absent from the `leader` foreground result. The updated
  `process_exec_normal_completion_leaves_outliving_descendant_alone` pin covers
  the real RPC path. Durable long-running output belongs to `background=true`.

- **#80 terminal run completion coupled to aggregate session Idle** — the
  earlier P1 `NO_SHIP` attribution was wrong. Lane C1 (`1cac39b`) proved the
  original hang was a test client waiting through a correlated capability
  error, then fixed the delegated-child identity defect exposed by the corrected
  setup. On this merged base,
  `terminal_child_run_without_session_idle_still_releases_parent` and the full
  10/10 core-loop binary pass. P1 does not duplicate C1's repair.

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

- **#84 paused Tokio time races real OS process progress** — after E1 moved
  process-exit observation to a kernel-notification thread, a test runtime with
  `start_paused` can auto-advance a foreground command's 60-second Tokio wall
  deadline before the independently scheduled OS child or notification thread
  makes progress. The symptom is a false timeout and teardown sweep in a test
  that is meant to exercise natural completion. Real-process integration tests
  now use real time; paused time remains only where the test deliberately drives
  the wall bound. On Darwin, the sweep probe also confirms the
  leader's waitable state synchronously before interpreting `EPERM` as a
  zombie-only process group; `EPERM` for a live group remains an error.

- **#85 late cancellation retroactively relabels detached ownership** — a
  foreground leader can complete naturally and detach its process-group
  authority before output artifact ingestion finishes. If a later cancel or
  broker close is allowed to overwrite the terminal classification, the result
  becomes `Cancelled` even though an unmanaged descendant correctly survives;
  that makes a cancellation-shaped sandbox escape. Natural leader exit is now
  an explicit winning boundary: cancellation remains sticky only when teardown
  won first. The gated-CAS regression requires successful completion, no sweep,
  and descendant survival; its failure arm requires the real CAS error rather
  than a masked cancellation.

- **#86 exit-observer failure mistaken for natural leader completion** — an
  error from the kernel exit observer is not evidence that the leader exited.
  Falling through a foreground or background natural-completion branch can
  detach a still-live group; the background form can first hang forever while
  awaiting that live leader. Both error arms now begin the owned TERM → grace →
  KILL ladder before completion handling. Separate injected-observer-failure
  tests start TERM-ignoring descendants and require the typed observer error,
  no survival marker, and no remaining process group; the foreground form also
  pins its leaked/live flags.

- **#87 thread count is not a lifecycle phase fence** — observing a second
  thread proves only that some helper exists; it does not prove the JSONL
  output adapter has crossed daemon adoption and run acceptance. A steady-state
  thread guard must first observe a product-owned phase marker, then enforce its
  exact count throughout the bounded plateau. The CLI guard now gives the
  flushed `accepted` record a ten-second deadline before requiring main plus
  one `tokio-rt-worker` in every sample; its expected count remains two.
