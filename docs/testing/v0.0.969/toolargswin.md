# Lane 969 toolargs Windows gate fix

## Provenance and guard

The branch is `lane-969-toolargswin` at `44bc0d1`, the requested
`origin/wave-969` merge of `e4889c0`. Before any edit, Guard #77 ran as
`bash scripts/check-unsafe-counts.sh` and passed with `production=188` and
`test=16`. The worktree had more than 30,000 MiB free before every build.

`LANE-COMMON.md` and `LANE-BRIEF-toolargswin.md` remain untracked inputs and
are not part of this change.

## Result

The two `haider-cli` failures are load-sensitive test-budget defects exposed
by the extra real-daemon CLI e2e in `e4889c0`; the new tool-argument parser and
rejected-result flow cannot execute on either failing path. Both fake-provider
scripts emit `Hang` or ordinary text/finish steps and no tool call. The
`required_string`/`optional_string` predicates did not change, no Windows path
normalization changed, and `BrokerToolDispatcher::execute` is reached only
after a provider tool call.

The fix is cfg-neutral and preserves every product assertion:

- The unmet session-readiness barrier now uses the adjacent test's finite two
  second allowance instead of 50 ms. `wait_for_sessions_ready` starts that
  total deadline before opening the session-list watch
  (`crates/haider-client/src/observe.rs:862-888`), so expiry during a loaded
  Windows handshake truthfully returns `daemon_ready=false`. The assertion
  still requires exit 124, `daemon_ready=true`, generation, one ready session,
  and the timeout error. It now prints the whole snapshot on failure.
- The JSONL timeout pin now owns a derived 13 second caller budget: ten seconds
  for loaded-gate scheduling/admission, the existing one second provider
  terminal-delivery reserve, and two seconds observing the deliberately
  hanging stream. Its existing two second terminal grace fits inside the
  existing 60 second subprocess bound (`13 + 2 < 60`). The assertions still
  require exit 124, contiguous sequences, exactly one terminal, and
  `terminal_kind=timeout` / `error_code=timeout`. Stdout and stderr are now
  reported if the exit classification drifts.

No product deadline, exit-code mapping, Windows cfg, toolargs parser, or
rejected-tool-result behavior changed. The `e4889c0` contract remains pinned:
invalid model-authored arguments produce a rejected `tool_result`, the result
returns to the provider, and the turn continues.

## Exit 65 causal chain

The CI excerpt records only exit 65, not the terminal payload, so the precise
`ProviderTimeout` value is a source-derived inference. It is the only matching
accepted-run path:

1. `run_headless_inner` starts the two second wall deadline before
   connect/create/attach and serializes the same absolute deadline into the
   headless spec (`crates/haider-client/src/headless.rs:2608-2616,2781-2793`).
2. The worker derives the provider deadline from that absolute timestamp
   (`crates/haider-daemon/src/worker.rs:7220-7222,9666-9686`).
3. Provider open reserves `PROVIDER_DEADLINE_SAFETY_MARGIN=1s`; when loaded
   setup leaves no more than that reserve, it returns
   `DeadlineExhausted` before polling even the immediately ready fake-provider
   open (`crates/haider-provider/src/lib.rs:351-354,2263-2280`).
4. While the caller's absolute timestamp is still in the future, the daemon
   deliberately does not append `run_deadline_exceeded`; the early failure
   remains provider-owned
   (`crates/haider-daemon/src/worker.rs:785-820`).
5. Without either a forced client timeout or the durable deadline fact, the
   reducer retains `ProviderTimeout/Errored`, and CLI maps provider timeout to
   `EX_PROVIDER=65` (`crates/haider-cli/src/run.rs:1576-1588`).

On an unloaded host, fake-provider open is polled before the reserve, `Hang`
persists to the caller deadline, and the same assertion exits 124. Adding a
real-daemon toolargs test changed the concurrent scheduling pressure, not this
control flow.

## Windows process-cancellation adjudication

`cancelling_process_exec_kills_the_real_process_group` is an independent known
Windows process-start-under-load failure. It belongs to the
`haider-daemond` `core_loop_e2e_tests` binary; the brief's `haider-tools`
label describes the implementation reached by that integration test, not its
owning test target.

- `e4889c0^..e4889c0` is empty for `crates/haider-tools`, this test, its support
  code, process supervision, shell commands, and Windows job-object handling.
- The fixture supplies a valid nonempty Windows PowerShell `command`. The old
  and new `ProcessExec` parsing predicates and resulting operation are
  semantically identical. Moving the valid cached parse earlier inside the
  dispatcher cannot explain the failure.
- CI exhausted the startup barrier with `heartbeat_bytes=None`,
  `descendant_started=false`, `last_state=Streaming`, and `failure=None`.
  Cancellation is sent only after that barrier succeeds, so this run never
  exercised or contradicted the process-group kill assertion.
- The test source documents hosted-Windows process-creation starvation. Its
  92 second outer bound is registry #94 arithmetic over the 30 second
  PowerShell cold-start allowance, 60 second process wall limit, and two second
  kill grace. Its external-file wait services Ping/Pong every ten seconds, so
  registry #95 is satisfied and read-idle retirement is excluded.
- The proven win2 live-turn harness uses a Windows-only 240 second
  process-start observation plus bounded clean settlement and one fresh retry.
  Hardening this separate test belongs to that harness class, not toolargswin.

The CI runner executes crates sequentially and runs `haider-daemond` before
`haider-cli`, so the new CLI e2e could not load or leak into this earlier
failure.

## Citation audit

The brief's `cli_tests.rs:1489` and `cli_tests.rs:2523` citations were correct
at `44bc0d1`: they were respectively the `daemon_ready == true` and exit-124
assertions. After the diagnostic/budget edit they are at lines 1493 and
2530-2536. The `core_loop_e2e_tests.rs:2758` citation is correct for the
failure-only startup panic. The test's classification as a `haider-tools`
test was drifted/wrong: it is a `haider-daemond` integration test that reaches
the `haider-tools` process implementation.

## Verification

All Rust commands used the lane environment:

```text
RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

- Prebuilt `cargo build -p haider-cli -p haider-daemond --bins --locked`:
  passed. Fresh `target/debug/haiderd` is 184,627,648 bytes, above registry
  #64's 10 MiB floor.
- Focused `run_jsonl_timeout_has_one_distinct_timeout_terminal`: passed with
  exit 124 and the unchanged terminal assertions.
- Focused `session_readiness_and_resume_are_finite_event_driven_json_barriers`:
  passed with the unchanged readiness/resume assertions.
- Focused `run_model_tool_argument_shape_error_is_rejected_and_continues`:
  passed, preserving the merged toolargs contract.
- Full `cargo test -p haider-cli --locked --no-fail-fast` with
  `HAIDER_TEST_SIBLINGS_PREBUILT=1`: passed, 561 tests.
- Full `cargo test -p haider-tools --locked --no-fail-fast`: passed, 217 tests;
  one pre-existing manual real-hardware screenshot test remained ignored.
- `cargo clippy -p haider-cli --test cli_tests --locked -- -D warnings`:
  passed.
- Closing Guard #77: passed again at `production=188`, `test=16`.
- `cargo fmt --all -- --check`, `git diff --check`, locked Cargo metadata,
  unmerged-index/conflict-marker scans, and branch/base checks: passed.

Windows behavior is by source inspection; no Windows toolchain is available
locally.

## CI error registry walk

| Registry class | Result |
| --- | --- |
| #1-#19 | Checked: no public API, ownership, import, lint allowance, or formatting class changed; the Rust diff is confined to two existing black-box test budgets/diagnostics. |
| #20/#21/#48/#54 | No test was added, removed, ignored, or platform-gated. Full owner suites ran with the mandated 8 MiB stack, so the test ledger is unchanged. |
| #22-#44 | Checked: no subscriber, schema, provider catalog, filesystem, process runner, release, dependency, cfg-boundary, collection, or socket-path behavior changed. |
| #45/#77 | Unsafe-count guard passed before edits and is rerun in the closing pass; no unsafe code was added. |
| #46-#63 | Checked: no runtime-root, walker, lock, UI, CAS, roster, connection-liveness, return-type, or archive behavior changed. Every claimed CLI guarantee retains a named behavioral assertion. |
| #64/#67/#71/#72/#74 | Fresh siblings were prebuilt, `haiderd` exceeds 10 MiB, daemon-spawning tests used the prebuilt flag, native discovery stayed disabled, and tests retain isolated HOME/USERPROFILE/profile roots. |
| #65/#68-#78 | Exit and terminal checks remain typed; no product error is swallowed, executable path, workflow, wire projection, release, or shutdown ownership changed. |
| #79-#93 | Checked: no process ownership, detach, output-reader, paused-time, PID identity, staged publication, sparse-file, line-ending, or sampling behavior changed. The independent process-start failure is adjudicated, not hidden. |
| #94 | Fixed in test harness: 13 seconds is `10s scheduling/admission + 1s existing provider reserve + 2s hang observation`; the existing terminal grace gives `13 + 2 < 60s` outer subprocess bound. Readiness uses the adjacent finite 2s negotiated-watch barrier instead of an underived 50ms total budget. |
| #95 | No new external wait or negotiated connection behavior. The independent Windows process fixture already services Ping/Pong every 10s during its external-file wait. |
| #96-#98 | Provider terminal-delivery reserve, route attribution, and replay batching/durability are unchanged. The larger fixture budget keeps the test outside #96's reserve without reclassifying it. |

No new CI error class was discovered.

## Independent verification

The final verifier re-read the completed code and report, reran the three
focused CLI pins, checked that the changes are cfg-neutral and retain every
semantic assertion, audited the inferred exit-65 chain and independent
process-start classification, checked registry #94/#95, and repeated the
repository guards. Verdict: `SHIP`.

SHIP
