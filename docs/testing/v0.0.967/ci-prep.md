# CI preparation and registry audit

## Gate results

The repository guard ran before the family suites and passed with production
unsafe count 188 and test unsafe count 15. `git diff --check`, conflict-marker
inspection, and Rust 2024 formatting of the new Rust file passed. There are no
manifest, lockfile, workflow, contract-fixture, version, xtask, or baseline
changes.

The release-gate environment and one shared hermetic `HOME`/`USERPROFILE` were
used for every Cargo command. `df -m /` ran immediately before each Cargo
command and never fell below the 700 MiB stop threshold. The final clippy gate
had 4,481 MiB free. `cargo metadata --locked` passed, and `cargo tree -d
--locked` was reviewed; all duplicate major/version families predate this lane.

Verification results:

- New end-to-end binary: 8 passed and 2 failed. The failures are the dropped
  outliving-child process output described in `tool-calling.md` and the
  missing-Idle child-settlement production defect described in `subagents.md`.
- `haider-daemon --lib`: 826 passed, 3 ignored.
- `haider-daemond`: all existing tests pass when the two known new red tests are
  filtered; `ephemeral_liveness_tests` is 6/6 and `lifecycle_tests` is 37/37,
  including `idle_daemon_admits_zero`.
- `haider-rpc`: all tests and the wire-golden contract pin pass.
- Prebuilt `haiderd` and `haider` before the CLI suite; the binaries are valid
  arm64 Mach-O files of 172 MiB and 96 MiB respectively. `haider-cli` then
  passes with `HAIDER_TEST_SIBLINGS_PREBUILT=1`.
- The one requested workspace `cargo check --all-targets --locked` passes.
- The first workspace clippy pass found two test-only style diagnostics. After
  fixing them, the final `cargo clippy --workspace --all-targets --locked -- -D
  warnings` passes. The later deterministic descendant fixture and cancellation
  observer also pass a touched-test `-D warnings` clippy run.

The registry's referenced `$T/ci-prep.sh` is not present in this worktree and no
`$T` value was supplied, so its applicable checks were run directly. A
workspace-wide test command was not substituted because the lane brief
explicitly forbids `cargo test --workspace` and supplies the required family
list instead.

## Verify-until-SHIP

1. Verifier iteration 1: `SHIP`. It found the suite sufficient to catch the
   966-style tool and subagent failures.
2. After replacing a timing-sensitive descendant command with a deterministic
   nested-leader fixture and making manual-cancel observation durable-RPC based,
   verifier iteration 2: `SHIP`.

These verdicts assess regression-detection power. They do not override the
release `NO_SHIP` caused by the two production failures the suite exposes.

## §A–§D class audit

`checked` means the class was inspected and has no lane-introduced violation;
`gap` is intentionally not represented as a pass.

| Class | Result | Evidence |
|---:|---|---|
| 1 | checked | No shared production type changed; new test constructors compile workspace-wide. |
| 2 | checked | No API rename/signature change; every new call site compiled. |
| 3 | checked | Workspace check found no ownership errors. |
| 4 | checked | Tests use public RPC/fixture APIs only. |
| 5 | checked | Unix/Windows-only imports and commands are cfg-scoped in the new test. |
| 6 | checked | No production table/import/variant changed. |
| 7 | checked | No manifest/lock change; locked metadata/check/clippy pass. |
| 8 | checked | No mechanical production sweep. |
| 9 | checked | Final workspace clippy passes. |
| 10 | checked | No dead or unused helper remains. |
| 11 | fixed | Removed `useless_conversion` and replaced manual membership search in `core_loop_e2e_tests.rs:118,1221`. |
| 12 | checked | No new production argument list. |
| 13 | checked | No clippy type-complexity finding. |
| 14 | checked | New comparable types satisfy existing derives; no new public data type. |
| 15 | checked | No `.last()` sweep or affected iterator. |
| 16 | checked | No manual-range diagnostic. |
| 17 | checked | No new lock is held over await; clippy is clean. |
| 18 | checked | Test-only `expect` allow is singular; production remains unchanged. |
| 19 | checked | `rustfmt --edition 2024 --check` passes on the new Rust file. |
| 20 | checked | Tests were added, but the owner explicitly forbids the baseline bump in this lane. |
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
| 37 | checked | Unix/Windows command arms return the same fixture types and workspace check passes. |
| 38 | checked | Fleet assertions compare projected strings with strings. |
| 39 | checked | The new test file compiles under package and workspace all-target checks. |
| 40 | checked | No cfg-windows dependency error conversion added. |
| 41 | checked | Shared real-daemon support owns the platform endpoint; local macOS suite connects successfully. |
| 42 | checked | No launch latency assertion; all waits are behavioral deadlines. |
| 43 | checked | Real process spawning is exercised, but no descriptor-sweep implementation changed. |
| 44 | gap | Local macOS IPC passed inside this lane; unsandboxed and other-kernel reruns belong to orchestration/CI. |
| 45 | checked | No unsafe block added; unsafe-count guard passes. |
| 46 | checked | Shared daemon fixture creates and validates its own temporary runtime root. |
| 47 | fixed | A real ignored `target/` containing a 1 TiB sparse artifact is exercised below the provider boundary. |
| 48 | checked | This is a Cargo integration test, not a daemon source sibling module. |
| 49 | checked | No acknowledgement/pipeline logic change. |
| 50 | checked | No serialized-size pin added. |
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
| 62 | checked | No public return type changed. |
| 63 | checked | No platform archive tool. |
| 64 | checked | `file` reports valid Mach-O binaries; `haiderd` is 172 MiB, above the 10 MiB pin. |
| 65 | checked | Assertions use typed semantic outcomes, not raw errnos. |
| 66 | checked | No STT surface. |
| 67 | checked | `haiderd` and `haider` were prebuilt; CLI ran with the sibling-prebuilt flag. |
| 68 | checked | No swallowed error was hardened. |
| 69 | checked | No Windows executable discovery. |
| 70 | checked | No workflow trigger or dispatch. |
| 71 | fixed | New tests drive a real daemon, IPC, tool execution, journal, projection, and client-visible terminal result. |
| 72 | gap | The owner-mandated environment disables discovery; this suite does not claim the native credential path. |
| 73 | checked | No fixed-window source scan. |
| 74 | fixed | Every daemon/Cargo process uses one temporary machine-user home, not the developer home. |
| 75 | checked | No actor shutdown implementation change. |
| 76 | checked | Fork provenance is asserted through the RPC projection and child replay. |
| 77 | checked | `scripts/check-unsafe-counts.sh` ran before tests and exited 0. |
| 78 | checked | No release/tag dispatch. |

## §D addition from lane 967-A2

- **#79 process completion terminates an outliving background descendant** — a
  command leader can exit while its background child still owns stdout. The
  released `process_exec` returns immediately with only the leader bytes and
  terminates the child before it can write a durable marker and its output.
  Check: require the marker and real client-visible `leaderchild` bytes; a
  timeout, leader-only result, or killed descendant fails. The new
  `process_exec_drains_output_from_child_that_outlives_leader` test is red.
  Production repair belongs to the concurrent process lane.

- **#80 terminal run completion coupled to aggregate session Idle** — a child run
  can reach `Done` while another queued child run makes the session active.
  `mirror_until_child_terminal` nevertheless requires a later aggregate `Idle`
  fence before releasing the already-terminal report, so the parent waits
  forever. Check: queue a second child turn through real RPC while run one is
  active; require run one `Done` and parent continuation. The new
  `terminal_child_run_without_session_idle_still_releases_parent` test fails in
  a bounded isolated process. Production repair belongs to the concurrent
  child-lifecycle lane; this lane does not edit `delegation.rs`.
