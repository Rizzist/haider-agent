# CI-PREP — lane 968-int3

## Verdict and citation audit

The retirement red is case **(b)**: a paused-time test synchronization race,
not a deleg/maxcost production holder. The test creates no run, delegated
child, budget coordinator, open lease beyond the supervisor's own lease, or
pending durable fact. The retain quiescence predicate and retirement path are
unchanged by both later lanes. The observed interleaving is: idle timer fires,
the supervisor publishes `IdleExpired`, the manager sends `Retire`, the
supervisor checks durable quiescence and unregisters its lease, the supervisor
task exits, and only a later manager poll consumes the `JoinSet` result and
removes the slot. A fixed number of `yield_now` calls did not synchronize that
chain.

At the merged input head, the brief's `worker.rs:11401` assertion citation was
correct. After this lane's synchronization seam, the assertion is at
`crates/haider-daemon/src/worker.rs:11422-11426`; the exact-time observable
wait is at `:11411-11420`, and its manager publication is at `:3477-3492`. The changelog
pin is at `crates/haider-protocol/tests/schema_changelog_tests.rs:145-155`.
The old v0.0.967 citation to client `headless.rs:468-486` is numerically current
but historically drifted because that range now contains v0.0.968's `Budget`
variant at line 474. The new changelog entry therefore cites line 474 exactly.

## Verification

- `cargo test --no-fail-fast -p haider-protocol --locked`: PASS, including the
  exhaustive schema changelog pin.
- `cargo test --no-fail-fast -p haider-daemon --locked -- --test-threads=4`:
  PASS — 894 unit tests passed with three intentional live-provider ignores,
  103 session-hub integration tests passed, and smoke/state-machine/doc tests
  passed.
- Manager law module after the fix: 20/20 at `--test-threads=1`, 20/20 at
  `--test-threads=8`, and 20/20 at each thread count while four concurrent
  `yes > /dev/null` CPU hogs ran.
- Delegation-filtered daemon tests: 9/9 PASS. `run_budget_tests`: 28/28 PASS.
- `cargo clippy -p haider-protocol --all-targets --locked -- -D warnings` and
  the equivalent daemon command: PASS.
- `scripts/check-unsafe-counts.sh`: PASS (`production=188`, `test=16`). Locked
  metadata, Rust 2024 formatting, `git diff --check`, unmerged-index scan, and
  conflict-marker scan: PASS.
- `cargo run -p xtask --locked -- test-count`: 4285 equals baseline 4285. No
  test was added, so no baseline update was required.
- Prebuilt `haider` and `haiderd` are arm64 Mach-O files; `haiderd` is
  182,264,320 bytes, above the 10 MiB integrity floor.

## §A–§D registry walk

| Class | Result |
|---:|---|
| 1 | checked: no production type constructor or exhaustive-match change. |
| 2 | checked: no `String`/`Vec`/`Option` API drift. |
| 3 | checked: no moved-then-used value; daemon check/tests/Clippy pass. |
| 4 | checked: the new observer remains private and `cfg(test)`. |
| 5 | checked: no platform-narrow import or use was added. |
| 6 | checked: no import or enum variant was added in code. |
| 7 | checked: no manifest/lock edit; locked metadata and gates pass. |
| 8 | checked: the focused diff is idempotent and contains no sweep. |
| 9 | checked: deny-warnings Clippy reports no collapsible flow. |
| 10 | checked: the test-only `Notify` and wait helper are both used. |
| 11 | checked: deny-warnings Clippy reports no combinator/borrow/default issue. |
| 12 | checked: no function argument list changed. |
| 13 | checked: no new complex type alias need. |
| 14 | checked: no equality derivation changed. |
| 15 | checked: no iterator terminal operation changed. |
| 16 | checked: no range expression changed. |
| 17 | checked: no mutex guard crosses an await. |
| 18 | checked: no lint attribute or production unwrap/expect was added. |
| 19 | fixed: Rust 2024 formatting passes for `worker.rs`; docs diff is clean. |
| 20 | checked: no test added; test-count remains 4285/4285. |
| 21 | checked: all test runs used `RUST_MIN_STACK=8388608`. |
| 22 | checked: no process-global subscriber/state installation. |
| 23 | checked: no migration changed; schema version remains 1. |
| 24 | checked: provider catalog authority untouched. |
| 25 | checked: no render benchmark or measurement change. |
| 26 | checked: no filesystem or platform path change. |
| 27 | checked: Windows wire semantics untouched. |
| 28 | checked: no process-tree test runner change. |
| 29 | checked: autospawn policy untouched. |
| 30 | checked: no external terminal-signal observer changed. |
| 31 | checked: Android untouched. |
| 32 | checked: no release rerun/tag action. |
| 33 | checked: no repository runner behavior changed. |
| 34 | checked: no dependency/module feature changed. |
| 35 | checked: no ambiguous trait method call. |
| 36 | checked: no temporary borrowed through `?`. |
| 37 | checked: no cfg-boundary type changed. |
| 38 | checked: no collection key changed. |
| 39 | checked: full daemon unit and integration tests compile and pass. |
| 40 | checked: no Windows dependency-error conversion. |
| 41 | checked: no UDS path or endpoint change. |
| 42 | checked: no cold-binary timing assertion. |
| 43 | checked: no descriptor sweep. |
| 44 | checked: no socket-behavior claim depends on the sandbox. |
| 45 | checked: unsafe-count guard passes; no unsafe code added. |
| 46 | checked: runtime-root derivation untouched. |
| 47 | checked: no filesystem walker change. |
| 48 | checked: no test source file or module declaration was added. |
| 49 | checked: no queued-batch acknowledgement path changed. |
| 50 | checked: no serialized byte pin changed. |
| 51 | checked: profile-lock behavior untouched. |
| 52 | checked: help viewport untouched. |
| 53 | checked: runtime-root permissions untouched. |
| 54 | checked: complete daemon suite used the CI 8 MiB stack and reached every binary. |
| 55 | checked: no cfg-Windows unit-valued binding. |
| 56 | checked: no deadline exit-code mapping changed. |
| 57 | checked: no UI layout pin changed. |
| 58 | checked: CAS inline threshold untouched. |
| 59 | checked: roster grammar untouched. |
| 60 | checked: Windows connection liveness untouched. |
| 61 | fixed: the test now enforces the documented manager-owned JoinSet fence. |
| 62 | checked: no public return type changed. |
| 63 | checked: no platform archive tool. |
| 64 | checked: prebuilt `haiderd` is valid Mach-O and 182,264,320 bytes. |
| 65 | checked: no raw errno or typed close outcome changed. |
| 66 | checked: STT untouched. |
| 67 | checked: `haider`/`haiderd` prebuilt and sibling-prebuilt flag used. |
| 68 | checked: no swallowed error was hardened. |
| 69 | checked: no executable path construction. |
| 70 | checked: workflow triggers untouched. |
| 71 | checked: production artifact behavior was not changed or claimed. |
| 72 | checked: credential discovery untouched; hermetic test env used. |
| 73 | checked: the changelog pin remains exhaustive; no fixed byte window added. |
| 74 | checked: no machine-user-global subsystem or subprocess fixture change. |
| 75 | checked: no hub-owned receiver or shutdown ownership change. |
| 76 | fixed: the additive terminal and nested decision schema are now documented at `docs/event-schema-changelog.md:124-149`. |
| 77 | checked: repository unsafe guard ran before final handoff. |
| 78 | checked: no tag push or release dispatch. |
| 79 | checked: no durability-policy helper changed. |
| 80 | checked: no child/session terminality observer changed. |
| 81 | checked: no process output-reader boundary changed. |
| 82 | checked: no foreground/background process ownership changed. |
| 83 | checked: no process detach fallback changed. |
| 84 | checked: no real process was introduced into paused time. |
| 85 | checked: cancellation classification untouched. |
| 86 | checked: no process exit-observer arm changed. |
| 87 | checked: accepted-run fencing untouched. |
| 88 | checked: no staged-file publication path. |
| 89 | checked: no Windows endpoint/path assertion. |
| 90 | checked: no sparse-file fixture. |
| 91 | checked: no source pin depends on line endings. |
| 92 | checked: no production maintenance counter/timer changed. |
| 93 | checked: no group-commit timing bound changed. |
| 94 | checked: no outer deadline was added; the mission-specific literal-yield race is fixed at `worker.rs:2515-2528`, `:3477-3492`, and `:11411-11420` with a release/acquire publication of the observable JoinSet/slot-removal event plus an exact virtual-time pin. |
| 95 | checked: no external-state wait or negotiated transport is involved. |

No new CI error class was exposed. The retirement failure is an instance of
the supplied paused-time synchronization class, so §D needs no new numbered
entry.
