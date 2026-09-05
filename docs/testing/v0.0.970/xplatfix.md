# v0.0.970 xplatfix

## Scope and claim audit

Base: `7431f8e6e9500729362cc4eb3cfb2bbc62cf462a`, lane
`lane-970-xplatfix`. This fixes release-blocking cross-platform CI failures;
it does not bump the workspace version. Read `LANE-COMMON.md`,
`LANE-BRIEF-xplatfix.md`, and both supplied turnperf evidence directories.
Those supplied files remain untracked and are excluded from this deliverable.
Historical turnperf estimates are not new performance evidence for this lane.

Read the actual failed logs using `gh run view <id> --log-failed`:

- [xplat 33950383012](https://github.com/Rizzist/haider-agent/actions/runs/33950383012),
  commit `368f093c`: Android, Windows check/clippy/tests, Linux clippy failures.
  Linux check and tests passed.
- [ci 33950382982](https://github.com/Rizzist/haider-agent/actions/runs/33950382982),
  same commit: one macOS test failure, during teardown.
- [ci 33953072908](https://github.com/Rizzist/haider-agent/actions/runs/33953072908),
  base `7431f8e6`: all three jobs passed, including full macOS tests. This is
  evidence of intermittency on the base, not verification of this patch.
- [xplat 33953072907](https://github.com/Rizzist/haider-agent/actions/runs/33953072907),
  base `7431f8e6`: completed with the same Android, Linux clippy, Windows
  compile/clippy and three Windows runtime failures; no additional failure class.
- `gh run view 33947869184 --log-failed` returned no log text; no panic is
  attributed to that empty result.

| Initial claim | Audit against logs and current source |
| --- | --- |
| Windows cannot find `haider_platform::process_exists` | Confirmed, but the actual caller is `crates/haider-cli/tests/turnhygiene_pin_tests.rs:225`, not monitorcore/toolshape product code. |
| Android arboard has eight errors | Confirmed: four E0433 and four E0425 for absent platform clipboard types. `.github/workflows/xplat.yml` Phase 1 builds CLI and daemon binaries, not a cdylib. CLI pulls in TUI and therefore arboard. |
| Linux clippy fails | Confirmed: `clippy::question_mark` in the Wayland clipboard fallback, old `clipboard.rs:301`. There are no other Linux clippy diagnostics in that run. |
| Parent-release regression fails on macOS | The named test fails, but all parent/child behavior assertions pass; the inner panic is `daemon shutdown stays bounded: Elapsed(())` at old `core_loop_e2e_tests.rs:4049`. The outer isolation assertion at line 1356 merely forwards it. |

Citations were located by symbol/content before use. The guessed Windows caller
and Android artifact kind were corrected. Old clipboard line numbers drift as
cfg/error handling is added. No historical turnperf source line is relied on for
a product change here.

## Per-job causes and fixes

### Windows check

The CLI test calls a Windows liveness API which was never exported or defined.
`haider-platform::process_exists` now uses the existing typed
`windows_process_state`: PID zero and known exited/missing processes are false;
query errors conservatively retain possible liveness. It adds no unsafe code.
Three Windows tests cover the live current PID/zero, an exited child whose
handle is still retained, and deterministic missing/query-error result mapping.
Existing Unix behavior is unchanged.

### Windows clippy

`acp_tests.rs` imported `Duration`, `AcpChildReap`, `AcpLaunchSpec` and retained
`scratch_dir` outside their existing Unix-only subprocess test configuration.
The helper and its imports now have matching cfgs; no test is disabled.
The Windows-only monitor test module now uses the repository's existing scoped
`clippy::expect_used` test allowance; production lint policy is unchanged.

### Windows full tests

Three further failures were present behind the compile failure:

1. `two_aliases_build_two_independent_supervised_adapters` searched a Display
   path inside Debug output, which escapes Windows backslashes. It now compares
   each adapter's typed `profile_dir()` with its exact expected alias directory.
   Distinct-adapter and same-alias reuse assertions remain intact.
2. `daemon_compactor_fuses_provider_view_and_cache_attempt_publication` includes
   `worker.rs` and searches an LF-bearing exact source fragment. Windows checkout
   converts the source to CRLF unless pinned. `.gitattributes` now marks this
   source `text eol=lf`, matching existing byte-sensitive source pins. The test
   and product code are unchanged. An LF/CRLF transformation reproduces the
   original literal lookup as true/false respectively.
3. `monitor_cwd_ancestor_cannot_be_replaced_between_prepare_and_spawn` configured
   a Windows child suspended, but never registered its job and resumed it before
   waiting. The fixture now follows `MonitorChild::spawn` ownership: register
   the process group, wait, then release the group. It still requires command
   success and refusal of the ancestor rename, with added exit/stderr diagnostics.
   The old job spent 863 seconds in this binary; the status-only failure does
   not establish which external mechanism finally terminated that child.

Native Windows runtime behavior is by inspection until Windows CI executes the
patch. The explicit clipboard CI step and its test file are preserved.

### Android Phase 1

`haider-tui` has a default `desktop-clipboard` feature whose optional arboard
is a non-Android target dependency. All native backend use sites share that
configuration. Android reads return `ClipboardError::Unavailable`; the UI shows
an honest notice and preserves the draft. Android local clipboard copy declines,
leaving the existing terminal-copy fallback available. Supported desktop reads,
copy behavior, and their messages are preserved.

The Android CLI/daemon dependency tree contains TUI and contains no arboard;
the default Windows inverse tree still contains arboard through TUI. Tests cover
Android writer selection, typed unavailable UI behavior without losing the
draft, and the real backend-free implementation with `--no-default-features`.
The native Windows clipboard test declares `required-features` for its backend;
that feature remains enabled by default in the unchanged CI invocation.

### Linux clippy

Use `std::env::var_os("WAYLAND_DISPLAY")?` in the existing Option-returning
fallback. This is equivalent to the previous early-None branch and satisfies
Clippy without an allowance.

### macOS full tests

This is a fixture-created connection-drain race, not evidence that the parent
failed to consume the child's terminal. The fixture holds two attached clients
unread while awaiting shutdown under two seconds; the configured production
drain deadline is five seconds. A writer must finish its current frame before
sending ServerDraining. In the failed CI log, listener shutdown and one
`NoticeDelivered` already occurred: worker-manager, account, hook, and hub drain
had completed, leaving one connection unretired. TempDir ownership errors follow
panic/unwind and are secondary, not the first failure.

The unread teardown dates to `1cac39baf` (2026-08-30), before providerrebind and
journalview. Providerrebind adds a Welcome feature in `connection.rs`, but does
not change its drain path, runtime shutdown, or this fixture. The exact runner
socket and byte backlog were not logged; attribution of that particular backlog
is an inference supported by the following controlled reproduction.

Executed under ENV LAW:

- Original normal isolated wrapper: PASS, 2.47 seconds.
- Original direct binary: 6/6 PASS under host load approximately 15.7–16.6;
  inner test times 2.2–2.4 seconds. Ordinary local load alone did not reproduce it.
- Controlled old fixture: leave its provider and every body assertion unchanged;
  only after those assertions, send one SessionRead with a large echoed request
  ID, consume an 8 KiB prefix to witness the in-flight reply, then use the
  original teardown. FAIL, exit 101, 4.11 seconds: prefix marker, listener close,
  one NoticeDelivered, then the exact `daemon shutdown stays bounded: Elapsed(())`
  panic and secondary ownership-cleanup messages observed on CI.
- Identical controlled fixture with repair: PASS, 2.15 seconds, both connections
  NoticeDelivered, Graceful outcome, flush/close `barrier_breached=false`.

Two preliminary probes were rejected as evidence: oversized provider text hit
the body deadline; many queued reads caused an ordinary queue-error retirement.
Neither reproduced the CI teardown mechanism. The accepted single-response probe
avoids both confounders. Synthetic traffic is not in the delivered fixture.

The repair continuously consumes both clients to actual read EOF concurrently
with join, retaining the original two-second deadline and every body assertion.
It additionally requires exactly one drain notice per client and a Graceful
outcome (the old test accepted Forced). The entire isolated fixture is bounded
by eight seconds, below the ten-second keepalive cadence, so its final reads
cannot owe a new keepalive. No sleep, product timeout, or teardown guarantee is
weakened. Independent review corrected the initial use of a keepalive helper
whose failed late Ping could otherwise masquerade as EOF.

## Verification environment and evidence

All Cargo builds/checks/tests use the lane ENV LAW:
`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`,
with two build jobs. Every build-capable command checks `df -m /` first and
refuses less than 700 MiB. Sibling binaries are freshly prebuilt before setting
`HAIDER_TEST_SIBLINGS_PREBUILT=1` for daemon/workspace tests.

The shell initially selected Homebrew Rust 1.95.0, whose sysroot does not include
rustup targets. That first Linux attempt failed with E0463 and was interrupted;
it is not a product failure. Subsequent commands explicitly select rustup's
1.95.0 toolchain. `rustup target add x86_64-unknown-linux-gnu
x86_64-pc-windows-msvc aarch64-linux-android` completed successfully.

Full command output is retained locally in `/tmp/xplatfix-evidence/`.
Executed on the merged `9270f402` tree:

| Command/job | Local result and scope |
| --- | --- |
| `cargo check --workspace --all-targets --locked --keep-going --target x86_64-unknown-linux-gnu` | Executed, exit 101: aws-lc/ring/SQLite need `x86_64-linux-gnu-gcc`; ALSA pkg-config has no cross sysroot. |
| Same Linux command with `clippy` and `-- -D warnings` | Executed, exit 101, same native dependency blockers. |
| Same workspace check and clippy for `x86_64-pc-windows-msvc` | Both executed, exit 101: missing `ml64.exe` and Windows SDK C headers (`stdlib.h`, `windows.h`, `assert.h`) in native dependency builds. |
| Same workspace check and clippy for `aarch64-linux-android` | Both executed, exit 101: missing NDK `aarch64-linux-android-clang`. No arboard is built. |
| Exact Android Phase 1 `cargo build --locked --target aarch64-linux-android -p haider-cli -p haider-daemond` | Executed, exit 101, missing NDK compiler. The workflow's Kotlin compile was not executed: Android SDK/Gradle are absent. |
| `cargo clippy -p haider-platform --all-targets --locked --target x86_64-unknown-linux-gnu -- -D warnings` | PASS, actual Linux target including platform test source. This does not compile the TUI Wayland clipboard branch. |
| Same platform-only clippy for Windows and Android with `--features blake3/pure` | Both PASS. Uses the dependency's supported pure-Rust hash backend to avoid its assembler/NDK build. Compiles actual Windows API/test code and Android platform code; this is supplemental evidence, not a default workspace pass or native test execution. |
| Default platform-only Windows/Android clippy | Both executed but blocked by BLAKE3's native assembler/compiler; reported separately from the pure-backend checks. |
| `cargo tree --locked --target aarch64-linux-android -p haider-cli -p haider-daemond` | PASS: TUI present, arboard absent. Default Windows inverse tree confirms arboard remains enabled. |
| Fresh merged CLI/daemon prebuild | PASS; haider 111,523,248 bytes, haiderd 201,304,736 bytes (>10 MiB). |
| Clipboard regressions with default features | PASS, 27 tests: Android policy/notice 2, composer 21, copy/clipboard contract 4. |
| `cargo test -p haider-tui --locked --no-default-features --test w970_android_clipboard_tests` | PASS, 3 tests, including actual OsClipboard typed unavailable implementation. |
| Fixed normal no-Idle isolated fixture | PASS, 2.17 seconds, no synthetic backpressure request retained. |
| `cargo run -q -p xtask -- test-count --update` | PASS: initial base 4,944 → upstream customprov 4,966 → merged lane 4,972 (+6 source markers: 3 Windows, 3 clipboard). This is not a count of tests executed on Mac. |
| Unsafe-count guard | PASS, 189 production / 20 test unsafe blocks, unchanged. |
| Release-packaging Python unittest | PASS, six tests executed on Mac. Native Windows execution is not claimed. |

The suggested Rust target installation does not supply native C compilers,
assembler, SDK headers, or pkg-config sysroots: Cargo still runs those dependency
build scripts during check/clippy even when no final Rust executable is linked.
No native Windows/Linux test binary, the separate native Windows clipboard job,
Android Kotlin build, or Linux X11 E2E was executed on this Mac. The corresponding
runtime behavior remains by inspection until CI runs the patch. In particular,
the Linux TUI `var_os(...)?` lint repair is source-reviewed but was not reached by
the cross-workspace Clippy attempt because native dependencies failed first.

Final merged-tree macOS gate: `cargo test -q --workspace --no-fail-fast`
PASS (5,374 top-level tests plus 12 nested subprocess probes, zero failures,
13 pre-existing ignores); `cargo clippy --workspace --tests -- -D warnings`
PASS; `xtask test-count` confirms 4,972/4,972; `xtask check`, `cargo fmt
--all --check`, and the unsafe-count guard all PASS. The unsafe totals remain
189 production and 20 test blocks. The repository guard reports nine existing
soft line-count warnings and exits zero. Local tests use four test threads for
the shared host; no assertion/deadline or CI runner configuration changes.

## Merge and commit

The original worktree's Git metadata is outside the writable sandbox. Real
`git fetch origin wave-970` and `git merge --no-commit origin/wave-970` attempts
were refused at `FETCH_HEAD` and `ORIG_HEAD.lock`; no permissions were changed.
The first read-only remote check matched base `7431f8e6`, but the final pre-gate
check found `wave-970` had advanced to `9270f402` (customprov).

Created a writable shared clone at `/tmp/xplatfix-integration`, on branch
`lane-970-xplatfix`, committed the lane there without a trailer, and performed a
real `git merge --no-commit 9270f402`. Git cleanly merged the one shared file,
TUI `runtime.rs`, preserving both custom-provider and clipboard changes.
Copied all 50 changed paths back to this worktree; the 1,855-file incoming-tree
check found zero missing files, including all nine newly added upstream files.
The user-supplied lane/evidence documents were not copied into the commit.

Final gates run against these merged files with freshly rebuilt siblings.
Original worktree HEAD cannot be advanced in this sandbox, and the orchestrator
owns the merge record and commit. No push is performed.

## Independent verification

Platform verifier: findings=1, real=1, noise=0. Its finding was that the new
optional backend left the native Windows integration test uncompilable with
`--no-default-features`. Adding `required-features = ["desktop-clipboard"]`
resolves that configuration and keeps the default Windows CI gate enabled;
locked Cargo metadata confirms selection. The verifier ran no runtime tests.

macOS verifier: findings=2, real=2, noise=0. The corrected drain helper reads
actual EOF rather than conflating a failed keepalive send with EOF. Its second
finding required the diagnostic probe to introduce backpressure only after all
body assertions and without flooding the request queue. The accepted single
large reply reproduces the exact teardown failure and passes with the repair.
Source-history wording was also corrected during review; it is not a separate
code/test/verdict finding. Aggregate: findings=3, real=3, noise=0.

Continuation verifier: findings=1, real=1, noise=0. It found that the public
Windows liveness semantics claimed missing-PID and conservative query-error
mapping without deterministic coverage. A pure result-mapping seam and named
Windows-target test now pin absent, exited, live, missing-error, and permission-
error outcomes. Aggregate: findings=4, real=4, noise=0.

Final completed-tree verification: code verifier findings=0; evidence verifier
findings=1, real=0, noise=1. The evidence verifier initially inspected the older
12:16 Windows cross-Clippy log and reported the 12:39 test edit as uncompiled;
the finding was rejected after it inspected the post-edit 12:42
`platform-x86_64-pc-windows-msvc-pure-clippy-final.log`, which passes. Overall
aggregate: findings=5, real=4, noise=1.

Release acceptance is proven only when `ci` and `xplat-check` are green on the
landed `wave-970` commit. Local Mac checks or a green run on the base cannot
establish that acceptance.
