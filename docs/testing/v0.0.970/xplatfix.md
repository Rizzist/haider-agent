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

## Round 2 — Windows test job

Base: `471b9d680610b62c4cdd4a8be7b6ee7faf3959d3`. Final forward-merge
target: `f211be0e9fb6ca960d0fa73e0dbc970f2a04fb37` (agentcli).
Read the supplied lane instructions and turnperf/turnperf2 evidence before
implementation. Those historical performance estimates are not Windows timing
measurements and those supplied documents are excluded from the commit.

### Claim audit

Read the complete log with `gh run view --job 101322816225 --log`, from
[xplat-check 33972289990](https://github.com/Rizzist/haider-agent/actions/runs/33972289990/job/101322816225).
It contains six failing tests in **three crates / four test binaries** (daemon
lib and daemon integration tests are separate binaries), not four crates.
All six panic citations match the base. The CLI citations at 446 and 657 are
shared assertion/wait helpers rather than the named tests' definitions. The
sidecar test lives under daemon `tests/`, not `src/`. Symbols were located before
editing; source lines shift with these repairs.

1. **Instruct-pipe byte pin — confirmed, exact difference identified.**
   `worker.rs` advertises `process_exec.command.description` as
   `Exact shell program passed to /bin/zsh -c when available, otherwise /bin/sh -c`
   (78 bytes) or
   `Exact PowerShell program passed to the absolute System32 Windows PowerShell`
   (75 bytes). The expected Windows total was incorrectly 5,670; actual is
   5,667. The test now pins the platform-invariant 5,670 − 78 = 5,592 bytes and
   adds the JSON-escaped byte contribution of this actual schema field (excluding
   its two framing quotes). Thus a manual edit cannot silently leave a stale
   three-byte offset. POSIX remains 5,670; Windows becomes 5,667. The 50% reduction
   requirement, tool counts, semantics and validation constraints remain pinned.

2. **Verified provenance guard — underbudget proven; exact Windows stall not
   measured.** The original ten-second observation encloses `process_exec`
   with its default sixty-second wall budget. Its command is `echo verified`,
   valid for Windows PowerShell; execution is explicitly authorized and the
   graph guard is cfg-neutral. Graph-only sibling confirmations passed on this
   Windows run. The log records only `Elapsed(())` and test-completion timestamps,
   not this process's spawn/exit timestamps, so it cannot distinguish cold
   PowerShell latency from another stall. The helper now derives its one-process
   observer BudgetSum as 10s graph/journal + 30s existing cold-PowerShell fixture
   allowance (`haider-daemond/tests/support/mod.rs`) + 60s `ProcessBounds` wall +
   2s termination + 2s pipe drain + two 500ms workspace receipts = **105s**.
   The 30s component is inherited fixture policy, not a measurement from this
   runner. Graph-only fixtures retain ten seconds. No sleep or product deadline
   is added. Incremental journal observations record elapsed time and sequence,
   fail immediately on typed tool/run failure or an unexpected blocking menu,
   and print the reason and progress on exhaustion. All original process-signal
   provenance, slim result, replay and explicit abandonment assertions remain.
   Failure queues a best-effort drain wake; it does not claim completed cleanup.

3. **Native pipe I/O failure — the claimed retained old tail is contradicted by
   the log.** Actual bytes contain the correct durable user row and coverage 2,
   both in generation **1**; the injected stale file was generation **9** with
   seq 999. The actor's append only enqueues asynchronous sidecar maintenance.
   The original fixture removed the obstruction and wrote generation 9 without
   joining that writer. A first-touch generation-1 rebuild can therefore overwrite
   the fixture's stale file. The repair moves the named test into
   `pipe_native_tests.rs`, where the existing private constructor can retain the
   **same `Arc<PipeNativeWriter>`** across two hub lifetimes. It joins the first
   actor while the obstruction still exists and asserts actual dirty state before
   removing it. The second hub uses that same writer and must rebuild generation
   9 → 10, match the complete durable bytes, clear dirty state, and confirm exact
   coverage. Independent review strengthened the stale file to plausible coverage
   1 with incorrect text: bypassing dirty repair must now fail, whereas seq 999
   independently caused a rebuild even without dirty handling. The expected
   generation-10 result was not weakened. The product also now refuses to
   classify an observed non-directory `pipe` parent as a missing sidecar:
   Windows `ERROR_PATH_NOT_FOUND` maps to Rust `NotFound`, whereas an executed
   macOS open beneath a file returned ENOTDIR. Root-open error classification
   checks the parent on the error path and preserves other I/O failures. A
   portable regression supplies Windows' ambiguous NotFound for absent-parent,
   absent-leaf and obstructed-parent cases, plus a non-NotFound failure. Rust
   1.95's installed Windows error-mapping source was inspected; no Windows API
   was executed. This fixes the classification, but cannot eliminate arbitrary
   external file-creation races or uniquely reconstruct the failed run's timing.
   No file-handle, rename or CRLF failure is established by the old log.

4. **Provider request golden — confirmed, only one differing JSON value.**
   Parsed both complete CI bodies and recursively diffed them: the sole pointer
   is `/tools/7/function/parameters/properties/command/description`, exactly the
   platform manual from claim 1. The comparison validates the native description
   against explicit platform prose, then replaces its single serialized string
   with `<PLATFORM_PROCESS_COMMAND_DESCRIPTION>`. All other request bytes,
   ordering and formatting remain sensitive. The existing native cold/warm/budget
   byte comparisons run before this normalization. A new regression checks both
   platform descriptions normalize identically while a changed `maxLength`
   remains different. There is one golden, regenerated through `UPDATE_FIXTURES=1`.

5. **Resident hook — real fixture interpreter mismatch.** Windows hooks execute
   `cmd.exe /D /S /C`, while the fixture supplied raw PowerShell syntax. This is
   distinct from the `process_exec` interpreter. The fixture now explicitly
   invokes absolute System32 PowerShell with `-NoProfile -NonInteractive
   -EncodedCommand`, encoding its script as UTF-16LE/base64 so cmd cannot
   reinterpret script or capture-path bytes. This follows the existing daemon
   hook-test pattern and adds only the workspace base64 dev dependency. Discovery
   between resident runs, two captured lines, session/run identity and cwd-scoping
   assertions remain intact.

6. **Monitor cwd — ancestor protection already passed; child cwd needed a
   compatible path and identity-based verification.** The original failure
   occurs after the ancestor-rename denial succeeds. Its log omitted actual cwd,
   so it does not uniquely prove a PowerShell reset versus Windows short/long
   pathname spelling. Canonical Windows `\\?\` paths have documented child-tool
   compatibility limitations ([Rust canonicalize documentation](https://doc.rust-lang.org/stable/std/fs/fn.canonicalize.html),
   [Rust PowerShell cwd report](https://github.com/rust-lang/rust/issues/133553)).
   `WorkspaceDirectory::process_path` now constructs a DOS/UNC spelling, opens
   its full chain, and checks its file identity against the retained original
   handle before returning it. A changed identity or unsupported namespace fails
   closed. Original root-to-cwd handles still deny delete-sharing and remain alive
   through CreateProcess. The shared setter propagates errors through the normal
   effect finalizer for foreground and background processes; monitor preparation
   also propagates them. Two Windows platform tests cover drive/UNC conversion
   and rejection of trailing-dot normalization that names a different directory.
   The monitor test retains ancestor-swap denial, compares reported/expected
   directory identities (the runner uses 8.3 temp spellings), and requires a
   relative sentinel read from the prepared directory, with actual cwd/stderr
   diagnostics. This is a product-path repair plus stronger security evidence;
   native Windows execution remains required.

### Round 2 verification

All Cargo commands use the ENV LAW, rustup Rust 1.95.0, two build jobs, four test
threads, and `df -m /` checked before each build-capable command with a 700 MiB
minimum. CLI/daemon siblings are prebuilt before daemon/workspace tests and
`HAIDER_TEST_SIBLINGS_PREBUILT=1` is set only afterward. Logs are under
`/tmp/xplatfix-round2-evidence/`; raw Windows job log is
`/tmp/xplatfix-windows-job.log`.

Executed focused checks on macOS:

| Repair | Executed host evidence | Windows evidence |
| --- | --- | --- |
| Instruct-pipe pin | Named daemon lib test PASS, actual 5,670 bytes. | Pin/schema arithmetic inspected; affected-crate cross check/Clippy blocked in native dependencies. |
| Provenance guard | All nine `worker::g1_todo_runtime_tests` PASS (1.67s); named provenance confirmation observed at about 606ms on this Mac. | Interpreter/guard path inspected; exact runner latency unknown; cross check/Clippy blocked before daemon test compilation. |
| Native pipe | All four `pipe_native::pipe_native_tests` PASS. Dirty-branch mutant FAILS with stale generation 9 versus expected durable generation 10, exit 101, 0.18s; exact source restoration then PASS (0.15s). | Ambiguous NotFound classification executed through the portable seam on Mac; actual Windows API inspected only. |
| Request golden | `UPDATE_FIXTURES=1 cargo test -p haider-cli --test turnhygiene_pin_tests provider_request_body_is_budget_independent_and_matches_the_golden_ledger -- --nocapture` PASS. Regenerated JSON diff has exactly one field. Entire 12-test binary also PASS. | Parsed actual/golden CI bodies; sole platform-manual difference proved. Windows CLI test cfg inspected; cross check/Clippy blocked in dependencies. |
| Resident hook | Named test PASS in all 12 `turnhygiene_pin_tests` (7.16s). This executes the POSIX fixture on Mac. | cmd/PowerShell encoded fixture, stdin EOF and cwd assertions inspected; no Windows hook executed. |
| Monitor cwd | Host compilation and final workspace gate cover shared callers/Unix implementations. | Platform-only Windows check AND Clippy with `--all-targets --features blake3/pure` PASS, compiling the new helper and two Windows tests. Tools monitor/foreground/background Windows cfg inspected, not compiled past native dependency blockers or run locally. |

Exact affected-crate Windows attempts were both executed under ENV LAW:
`cargo check -p haider-platform -p haider-tools -p haider-daemon -p haider-cli
--target x86_64-pc-windows-msvc --all-targets --keep-going`, and the same
`cargo clippy ... -- -D warnings`. Both exit **101**: missing `ml64.exe` and
Windows SDK C headers (`stdlib.h`, `windows.h`, also native crypto/SQLite
requirements). These are not passing Windows checks. Supplemental
`cargo check -p haider-platform --target x86_64-pc-windows-msvc --all-targets
--features blake3/pure` and the same Clippy command with `-- -D warnings` both
exit **0**; they use BLAKE3's supported pure-Rust backend, not a fake SDK or
forced host cfg. They do not execute Windows binaries or compile the dependent
CLI/daemon/tools Windows paths.
These exact default and pure-platform commands were repeated on the resolved
`f211be0e` forward merge in the integration clone: default check/Clippy again
exit 101 in native dependencies, while both platform-only commands exit 0.
Their logs have the `merged-windows-` prefix.

The first focused G1 run found a new diagnostic-helper defect: blanket
`decode_event` rejected daemon-owned `project_instructions_loaded` facts outside
`EventPayload`. Restricting strict decoding to the six observed wire kinds while
advancing the cursor over every fact corrected it; the entire nine-test suite
then passed. This was found by execution, separately from the independent
verifier tally. No failed run is counted as a pass.

The first full workspace invocation stopped during test linking with
`errno=28 (No space left on device)`: the filesystem dropped to 139 MiB,
below the 700 MiB ENV LAW floor. No later build was started until space was
recovered. The failed log and status are preserved as
`workspace-test-unstripped-environment-blocked.log` and
`host-gate-status-unstripped-environment-blocked.tsv`. Only this lane's generated
`target/` tree was removed, recovering 8,592 MiB; no other lane's artifacts or
source files were deleted. The restarted gate retains every ENV LAW setting and
adds `CARGO_PROFILE_DEV_STRIP=symbols CARGO_PROFILE_TEST_STRIP=symbols` to bound
Mach-O artifact size. This strips binary symbols, changes no test assertions or
product source, and applies consistently to freshly rebuilt siblings and tests.

On the resolved forward merge, the named instruct-pipe test passed again at
**5,670 → 5,670 POSIX bytes** (derived Windows expectation 5,667). The provider
golden was regenerated and its named test passed again; its SHA-256 stayed
identical to the pre-regeneration merged fixture, so there was no additional
golden drift after the merge. Final workspace gate results follow below.

The first merged workspace test command returned zero, but its following
Clippy pass exposed stale pre-merge metadata: it reported missing upstream
`AgentSpawnSpecV1` / `HarnessConfig.agent_spawn` despite both being present in
the resolved source. Copying with preserved timestamps had put changed source
before the pre-merge Clippy fingerprints. That test pass is therefore not used
as final fresh-build evidence. Its logs/status are preserved under the
`merged-stale-metadata-` prefix. `cargo clean -p ...` removed only this
worktree’s workspace-package artifacts (all 17 workspace members), retaining
third-party dependencies. The complete sibling build, pins, baseline, workspace
test and strict Clippy gate were restarted; a fresh strict Clippy pass before
the expensive test run checks that the stale metadata failure is gone. That
fresh strict Clippy pass returned **0** in 3m 30s, with no source change needed.

The baseline before this round is
4,997 source test markers; the first `xtask test-count --update` produced
5,001. The forward merge supplies upstream's 5,027 baseline; recounting the
resolved tree produces **5,031**. Moving the sidecar test preserves its count,
while one normalizer, one missing-parent and two Windows directory tests add
four. This is distinct from the number of tests executed on Mac.

### Round 2 merge, commit and acceptance

The user explicitly requests a commit, overriding the older uncommitted-lane
instruction. The worktree's `.git` points outside the writable sandbox. Actual
fetch and merge attempts failed at `FETCH_HEAD` and `ORIG_HEAD.lock`. A writable
shared clone at `/tmp/xplatfix-round2-integration`, on `lane-970-xplatfix`, fetched
`origin/wave-970` and ran `git merge --no-commit origin/wave-970` before the gate.
Initial checks found `471b9d68` up to date. During the long host gate, the
remote advanced to `f211be0e` (agentcli). The lane changes were committed in the
clone as `f53c6d70`, then `git merge --no-commit origin/wave-970` performed the
actual forward merge. The shared session-hub test file merged cleanly, preserving
both the input-owner acknowledgement assertions and this lane's moved sidecar
regression. Registry-walk additions were unioned; the baseline conflict was
resolved by the repository test-count tool, not by choosing either side's number.
All 557 paths changed from the original base were copied back before the final
host gate; all 2,691 tracked files matched the resolved clone byte-for-byte. The clone is the
commit location; source changes remain present in the original worktree. No push.

A stop attempt for the superseded pre-merge TUI benchmark was refused by the
sandbox (`operation not permitted`); it was not stopped or disabled and completed
naturally in 630.86s. The pre-merge workspace test, strict Clippy, xtask,
formatting and unsafe gates all subsequently passed. Their logs are preserved
with the `premerge-` prefix; acceptance uses the complete gate on the merged
source tree.

Independent reviewers found two issues that changed tests: the dirty-tail
fixture could pass without dirty handling, and the monitor assertion compared
path spellings rather than directory identities. Both were repaired. A concern
that the guard would hide the failure reason behind Errored was rejected because
`RunFailed` precedes Errored atomically. A best-effort-cleanup comment was clarified;
that editorial correction is not counted as an additional behavioral finding.
A final read-only review against the forward-merged upstream found no new
material issues: all six repairs remained, and the shared session-hub file
preserved upstream assertions.
Round 2 aggregate: **findings=3, real=2, noise=1**.

**Release acceptance is proven only when xplat-check is green on the landed
`wave-970` commit.** A local Mac pass or Windows source inspection does not prove
that acceptance; this lane does not run Windows binaries locally or push CI.
