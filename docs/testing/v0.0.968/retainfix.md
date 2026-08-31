# v0.0.968 retainfix — diagnosis and verification

## Outcome

The three reproducible test defects are fixed without widening a sleep or
weakening a product invariant. The fourth report was stale against the requested
`origin/wave-968` head (`b0fc75d`): the product deadline wake was already present
and working, so this lane adds the missing short-TTL, real-CLI regression pin
without speculative runtime churn. Every item received an independent `SHIP`
verdict before work began on the next item.

The branch and base were checked before editing: local `lane-968-retainfix`,
`HEAD`, and `origin/wave-968` all named `b0fc75d2aa181086026c83a2c08e5c244df9959a`.
The initial registry #77 guards passed.

## Citation audit

The brief's line numbers came from an older tree. Each construct was located by
name before use.

| Brief citation | Verdict | Current evidence |
|---|---|---|
| retain-manager TTL test | correct construct, line drift | `crates/haider-daemon/src/worker.rs`, `manager_law_tests::durably_quiescent_supervisor_retires_at_the_conservative_idle_ttl` |
| `crates/haider-daemond rpc.rs:2052/:2205` | wrong crate and drifted lines | The soak is in `crates/haider-daemon/src/session_hub/rpc.rs`; the cited sites were test assertions, not production unwrap/expect paths. |
| `process_tools_tests.rs:1625` | correct file, line drift | `crates/haider-tools/tests/process_tools_tests.rs`, `dropping_process_execution_cancels_and_kills_the_child_group` |
| idle-exit path | report stale on requested base | Env parsing is in `haider-cli/src/run.rs`; the daemon deadline arm/wake is in `haider-daemon/src/runtime.rs`, including the `wait_for_idle_linger` select branch introduced by ancestor `1e9b355`. |

## 1. Retain TTL Linux flake

The manager was correct. The paused-time test used 32 `yield_now` calls as a
scheduler fence before `tokio::time::advance`. On a loaded Linux runner, the test
could advance virtual time before the supervisor re-polled and armed its fresh
timer. Tokio then auto-advanced the pending sleep by another full 300 seconds,
which explains the exact late-TTL failure.

The pin now uses paused-time sleeps. A sleep lets all ready tasks run before the
clock advances, so it deterministically observes the pre-activity boundary and
the complete fresh TTL. Registry #94 is explicit:

```text
1 ms + (SUPERVISOR_IDLE_TTL - 2 ms) + 1 ms = SUPERVISOR_IDLE_TTL
```

Proof:

- focused TTL pin: PASS;
- manager-law owner suite: 11/11 PASS at 1 and 8 test threads;
- direct pin under four CPU stressors: 100/100 PASS;
- independent verifier: `SHIP`.

## 2. Observe soak panic

Archived failures had zero supervisor slope, zero ObserveDigestCache entry
slope, and zero targeted-byte slope, but Linux RSS slopes of 22,554.691 and
33,119.621 bytes/session exceeded the old 16 KiB gate. Process RSS includes
SQLite page-cache and allocator high-water state; it is not an ownership proof.
The child first panicked on that RSS assertion, then the parent panicked again on
the child's exit status. The cited paths were therefore test harness panics, not
a production RPC unwrap/expect.

The soak now checks every one of its 72 deletions for the exact daemon-owned
retention surfaces: supervisors, ready/building observe entries, observe bytes,
and session actor tasks must all be zero. RSS and fitted slopes remain printed as
diagnostics only. Setup, child-launch, invariant, operation, and shutdown
failures return non-retryable `HaiderError { code: Internal }`. A separate pin
proves every nonzero owned-retention dimension produces that typed error.

Proof:

- typed error pin: 1/1 PASS;
- owning observe-retention module: 10/10 PASS;
- isolated soak in fresh child processes: 20/20 PASS;
- deletion-fence and observe-digest neighbors: 3/3 PASS;
- independent verifier: `SHIP`.

## 3. macOS tools drop ordering

The old mutation sentinel launched a shell loop that created a new `sleep 0.01`
descendant on every iteration, then sampled heartbeat growth after fixed 80 ms
and 100 ms sleeps. On loaded macOS that mixed the Drop handoff contract with
transient process-group churn; broker close could observe `drop-cancel` during an
escalation even though dedicated tests already cover descendant sweeping.

The pin now starts one `exec`'d Perl process with a finite alarm, captures its
original effect id, drops `ProcessExecution`, and waits for that exact durable
terminal outcome through a lost-wake-safe `Notify`. It requires `Cancelled` or
`CancelledEscalated` before `broker.close()`. There is no observation sleep.
Registry #94 is explicit:

```text
1,000 ms command alarm + 10 ms kill grace + 10 ms pipe-drain grace = 1,020 ms
```

Proof:

- exact pin: PASS;
- owner integration suite: 28/28 PASS;
- concurrent focused repetitions: 800/800 PASS;
- independent repetitions: 100/100 PASS;
- removing only the Drop cancellation send produced a non-cancelled outcome at
  the finite boundary and failed the pin; restoration returned it to green;
- independent verifier: `SHIP`.

## 4. Idle exit

The reported product defect is not present on the requested base. Ancestor
`1e9b355` already carries the complete path:

1. `HAIDER_RUN_DAEMON_IDLE_TTL_MS` selects `LingerIfSpawned`.
2. The launcher passes the private idle-linger argument to `haiderd`.
3. Launcher death requests `GracefulAfterIdle`.
4. Zero attached clients arms `now + TTL`.
5. The accept loop selects on `sleep_until(deadline)`, re-enters the loop, and
   terminalizes through normal graceful cleanup.

A fresh manual real-CLI run with a 50 ms TTL logged launcher exit, idle shutdown
arming, zero attached connections, the shutdown decision, and store/runtime
cleanup. Current-head Linux, macOS, and Windows liveness artifacts also pass.
The external orphan census cannot be attributed to current source from this
sandbox; a stale prebuilt binary is possible but unproven.

The new regression uses a fresh profile, actual `haider run`, actual `haiderd`,
the environment variable, a 250 ms TTL, and a retained kernel process identity.
It waits without polling and then requires endpoint cleanup. Registry #94 is:

```text
250 ms idle TTL + 5,000 ms daemon graceful-drain budget = 5,250 ms
```

Proof:

- new exact real-run pin: PASS;
- autospawn owner suite: 8/8 PASS;
- ephemeral launcher-liveness neighbors: 8/8 PASS;
- removing only the accept loop's idle-deadline select branch made the retained
  daemon survive the derived deadline and failed the pin; the branch was
  restored and `runtime.rs` has no diff;
- independent verifier: `SHIP`.

## Commit disposition

The task-specific brief asks for one evidence-bearing commit per item, while the
shared 968 rules say to leave work uncommitted for the orchestrator. This
worktree's Git metadata is also outside the writable sandbox. The attempted
item-1 commit was refused when Git could not create
`.git/worktrees/lane-968-retainfix/index.lock`; no commit or index mutation was
made. The isolated intended commits are:

1. `test: fence retain TTL re-arm deterministically`
2. `test: make observe retention soak owner-exact`
3. `test: await dropped process terminalization`
4. `test: pin real run idle daemon exit`

The evidence in the four sections above is the proposed body for each commit.

## Verification environment

Every Cargo invocation used:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Daemon/client subprocess suites also used
`HAIDER_TEST_SIBLINGS_PREBUILT=1` after both siblings were built. `df -m /` was
checked before every Cargo invocation and remained safely above the 700 MiB stop
threshold. `test-baseline.txt` moves from 4305 to 4307 for the item-2 and item-4
pins.

## Final frozen-diff gate

| Check | Result |
|---|---|
| retain manager-law owner suite | PASS, 11/11 |
| observe-retention owner module | PASS, 10/10; isolated soak again reported every owned dimension at zero |
| process-tools owner integration | PASS, 28/28 |
| real CLI autospawn owner integration | PASS, 8/8 |
| ephemeral launcher-liveness neighbors | PASS, 8/8 |
| deny-warnings Clippy | PASS for `haider-daemon`, `haider-tools`, and `haider-cli`, all targets |
| rustfmt | PASS, all crates |
| test ledger | PASS, 4307/4307 |
| unsafe-count guard | PASS, production 188 / test 16 |
| locked Cargo metadata | PASS |
| diff, conflict-marker, unmerged-index, and branch/base guards | PASS |
| built `haider` | Mach-O arm64, 102,857,840 bytes |
| built `haiderd` | Mach-O arm64, 184,377,152 bytes; exceeds 10 MiB |

## CI error registry walk

`checked: none` means the class was inspected against this exact diff and no
instance was introduced or exposed. `fixed` identifies the lane evidence that
directly exercises the law.

| Class | Result | Evidence |
|---:|---|---|
| 1 | checked: none | No public struct or enum shape changed. |
| 2 | checked: none | No public API or signature changed. |
| 3 | checked: none | Test-only ownership changes compile and are exercised by their owner suites. |
| 4 | checked: none | No visibility seam changed. |
| 5 | checked: none | No cfg-narrow import was added. |
| 6 | checked: none | The added `EffectId` import is unique and used. |
| 7 | checked: none | No manifest or lockfile edit; locked metadata resolves. |
| 8 | checked: none | Both mutations were restored and the final diff was re-read. |
| 9 | checked: none | Affected-target deny-warnings Clippy is part of the final guard. |
| 10 | checked: none | Every added helper and value is exercised. |
| 11 | checked: none | No combinator, cast, or return lint remains. |
| 12 | checked: none | Only private test-helper signatures changed. |
| 13 | checked: none | No type-complexity diagnostic. |
| 14 | checked: none | `SharedJournal` retains valid `Debug`/`Default` derivation. |
| 15 | checked: none | No iterator-end rewrite. |
| 16 | checked: none | No range rewrite. |
| 17 | fixed: process drop pin | The journal mutex is released before notification or await; the waiter is armed before inspection. |
| 18 | fixed: observe soak | Harness/invariant failures use typed `HaiderError`; no lint allowance or production unwrap was added. |
| 19 | checked: none | Rustfmt and diff checks are part of the final guard. |
| 20 | fixed: `test-baseline.txt` | Two pins move the ledger from 4305 to 4307. |
| 21 | fixed: all Cargo evidence | Every Rust test used the required 8 MiB stack. |
| 22 | checked: none | No process-global tracing subscriber changed. |
| 23 | checked: none | No migration or schema changed. |
| 24 | checked: none | Provider catalogs and authority are untouched. |
| 25 | fixed: observe soak | RSS is retained as a diagnostic, not misrepresented as owned-retention correctness. |
| 26 | checked: none | No production filesystem/platform API changed. |
| 27 | checked: none | No Windows wire behavior changed. |
| 28 | checked: none | Process runner behavior is unchanged. |
| 29 | checked: none | Autospawn product policy is unchanged; the real path gains a black-box pin. |
| 30 | fixed: observe/drop pins | Async terminal waits are finite or driven by an exact durable terminal fact and report actual state. |
| 31 | checked: none | Android and release artifacts are untouched. |
| 32 | checked: none | No release action occurred. |
| 33 | checked: none | No runner behavior changed. |
| 34 | checked: none | No dependency or feature was added. |
| 35 | checked: none | No ambiguous trait call. |
| 36 | checked: none | No temporary is borrowed through `?`. |
| 37 | checked: none | No cfg-boundary type changed. |
| 38 | checked: none | No collection-key seam changed. |
| 39 | checked: none | New helpers live inside existing declared test targets. |
| 40 | checked: none | Typed error conversion is platform-independent. |
| 41 | checked: none | Existing short/private profile roots are reused. |
| 42 | checked: none | No cold-launch timing assertion was added. |
| 43 | checked: none | No descriptor sweep changed. |
| 44 | fixed: idle-exit pin | A real local daemon/client IPC path starts, serves a turn, exits, and cleans its endpoint. |
| 45 | checked: none | No unsafe code was added; production/test unsafe counts remain guarded. |
| 46 | checked: none | Runtime-root derivation is unchanged. |
| 47 | checked: none | No filesystem walker changed. |
| 48 | fixed: existing test targets | Two new tests are reflected in the test ledger. |
| 49 | checked: none | No queued acknowledgement path changed. |
| 50 | checked: none | No platform-dependent serialized-byte pin changed. |
| 51 | checked: none | Profile-lock behavior is untouched. |
| 52 | checked: none | TUI viewport behavior is untouched. |
| 53 | checked: none | Existing test-home/runtime isolation is preserved. |
| 54 | fixed: all Cargo evidence | Correct runner stack was exported before every suite. |
| 55 | checked: none | No cfg-Windows unit binding changed. |
| 56 | checked: none | Product deadline reasons and terminal mapping are unchanged. |
| 57 | checked: none | No UI layout pin changed. |
| 58 | checked: none | CAS thresholds are untouched. |
| 59 | checked: none | Roster rendering is untouched. |
| 60 | checked: none | Connection-liveness production code is unchanged. |
| 61 | fixed: all four pins | Every claimed guarantee has a behavioral assertion. |
| 62 | checked: none | No public return type changed. |
| 63 | checked: none | No platform archive utility was introduced. |
| 64 | fixed: sibling inspection | Built `haiderd` exceeds the 10 MiB truncation sentinel. |
| 65 | checked: none | Assertions use typed outcomes/identity, not raw errno. |
| 66 | checked: none | STT is untouched. |
| 67 | fixed: subprocess suites | Siblings were prebuilt and the required flag was exported. |
| 68 | checked: none | No product error is swallowed. |
| 69 | checked: none | No executable path-casing logic changed. |
| 70 | checked: none | No workflow trigger or dispatch changed. |
| 71 | fixed: idle-exit pin | The real built pair completes a fake-provider turn and daemon cleanup. |
| 72 | checked: none | Native discovery stayed disabled while the explicit fake provider was armed. |
| 73 | checked: none | No fixed-window source scan was added. |
| 74 | checked: none | Real-daemon tests use an isolated HOME/USERPROFILE and profile directory. |
| 75 | checked: none | Hub shutdown ownership is preserved and exercised. |
| 76 | checked: none | No wire projection changed. |
| 77 | fixed: repository guards | Base, unsafe counts, locked metadata, formatting, diff, conflict, index, and binary guards are recorded. |
| 78 | checked: none | No tag or release dispatch occurred. |
| 79 | fixed: process drop pin | The exact original effect reaches durable cancellation before broker teardown. |
| 80 | checked: none | Process exit, not session idleness, is the idle-daemon terminal fact. |
| 81 | checked: none | No output-reader readiness heuristic was added. |
| 82 | fixed: process drop pin | Drop transfers cancellation ownership and waits on the durable broker outcome. |
| 83 | checked: none | No detach-failure behavior changed. |
| 84 | fixed: retain TTL pin | Paused Tokio time fences ready work; it is not used to drive an OS process. |
| 85 | fixed: process drop pin | Only typed cancelled terminal outcomes satisfy the Drop contract. |
| 86 | fixed: idle-exit pin | `ProcessExitMonitor` retains the exact kernel identity against PID reuse. |
| 87 | fixed: retain TTL proof | Manager law passes at both 1 and 8 test threads and under CPU contention. |
| 88 | checked: none | No staged manifest publication changed. |
| 89 | checked: none | The new autospawn test is Unix-only with no Windows filesystem claim. |
| 90 | checked: none | No sparse-file fixture changed. |
| 91 | checked: none | No line-ending-sensitive source assertion was added. |
| 92 | fixed: retain TTL pin | The maintenance timer re-arm is fenced by scheduler-aware paused-time sleeps. |
| 93 | checked: none | RSS samples remain diagnostic; no process-throughput sampler was added. |
| 94 | fixed: items 1, 3, and 4 | Every new deadline is the named sum of the budgets it wraps; no double-until-green. |
| 95 | checked: none | No external-state wait holds an open negotiated connection without keepalive. |
| 96 | checked: none | Provider terminal-delivery reserve logic is untouched. |
| 97 | checked: none | Route attribution logic is untouched. |
| 98 | checked: none | Replay batching/durability logic is untouched. |

No new CI error class was discovered by this lane.
