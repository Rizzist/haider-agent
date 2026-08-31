# Max-cost lane CI preparation and registry audit

## Scope and citation audit

The brief's citations were audited against base `8952219`, not trusted by line
number. All four were correct on that base: CLI parsing at `run.rs:176-181`,
the protocol field at `headless.rs:27`, the post-exchange worker check at
`worker.rs:8759`, and the zero guard at `session_hub/rpc.rs:12847`. The worker
location has drifted in this lane because admission now occurs at the physical
provider-request seam; the other cited constructs remain present.

This lane changes only the run-budget path and its protocol, client/CLI
projection, tests, and contracts. It does not change delegation waits,
worker-supervisor retirement, provider resume transport, OAuth, hooks, or the
workflow-continuation implementation.

## Verification evidence

Every Cargo command used the release-gate environment and was preceded by
`df -m /`; free space remained above 31 GiB. Daemon and CLI tests also used
`HAIDER_TEST_SIBLINGS_PREBUILT=1`.

- Complete `haider-daemon` test run: 874 passed / 3 ignored in the library,
  103/103 session-hub integration tests, and both one-test integration binaries
  passed.
- Complete `haider-core` test run: all test binaries passed (one manual timing
  probe ignored).
- Complete `haider-protocol`, `haider-client`, and `haider-cli` test run: all
  test binaries passed, including 114/114 CLI integration tests and 30/30
  headless-client tests.
- Affected-crate all-target Clippy with `-D warnings` passed for
  `haider-protocol`, `haider-core`, `haider-client`, `haider-cli`, and
  `haider-daemon`.
- The unsafe-count guard passed at production=188/test=15. Locked metadata,
  Rust formatting, `git diff --check`, conflict-marker inspection, workflow
  YAML parsing, and the xtask repository check passed. There are no changed JSON
  fixtures, Cargo manifests, dependency edges, or lockfile entries.
- The test ledger was updated from 4082 to 4254 and rechecked at exactly 4254.
- Fresh `haider` and `haiderd` builds are arm64 Mach-O files of 102,635,984 and
  181,298,496 bytes. `haiderd` exceeds the required 10 MiB integrity guard.

The required mutation was executed: removing only the pre-transport projected
budget check made
`projected_first_request_over_cap_sends_zero_provider_requests` fail with one
observed provider request instead of zero. Restoring the check made the exact
test pass.

## Verify-until-SHIP history

1. Research review: `NO_SHIP`. It found ambiguous projected-value semantics,
   incomplete terminal detail, an unknown-price reconciliation defect, a
   vacuous actual-usage test, and protocol tests in the wrong module form.
2. Independent verification iteration 1: `NO_SHIP`. It found uncovered exits
   after a sent request (subturn, cancellation, and restart), a non-null time
   projection, unknown-price precedence, and missing max-time/compaction pins.
3. The budget decision now uses incremental projection, every post-send exit
   reconciles or fails closed as `usage_unavailable`, durable request attempts
   detect restart gaps, unknown pricing wins under a cost cap, time has no
   projection, and focused tests cover each case. Native PDF request bytes are
   counted per block occurrence. Compaction's two provider sends have a
   brace-bounded admission-before-transport mutation pin.
4. Final independent verification: both budget-path and contract verifiers
   returned `SHIP` after re-reading the corrected tree and rerunning focused
   tests, formatting, Clippy, contract, ledger, and artifact checks.

## Registry §A–§D walk

`checked` means the class was inspected and has no lane-introduced violation.
`fixed` names a concrete lane repair or guard.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed | All constructors and matches for the additive decision detail and `HeadlessTerminalKind::Budget` were reconciled and compiled. |
| 2 | fixed | Provider-budget guard signatures and every actor/compactor caller were reconciled. |
| 3 | checked | Affected all-target check/Clippy found no moved-use defect. |
| 4 | checked | Tests use public or crate-visible constructors; no private test-field reach-through. |
| 5 | checked | No cfg-narrow import was introduced. |
| 6 | checked | No duplicate imports, variants, or wire table rows. |
| 7 | checked | No manifest or lockfile change; locked metadata succeeds. |
| 8 | checked | Mutation and restoration were re-read, formatted, and rerun; the sweep is not mechanical. |
| 9 | checked | Deny-warnings Clippy reports no collapsible branch. |
| 10 | checked | No dead or unused budget helper remains. |
| 11 | checked | Deny-warnings Clippy reports no combinator/cast/borrow family issue. |
| 12 | checked | New multi-value internals use typed structs rather than an overlong public argument list. |
| 13 | checked | No type-complexity diagnostic. |
| 14 | fixed | Decision/reason types derive the comparison traits supported by every field. |
| 15 | checked | No double-ended iterator rewrite. |
| 16 | checked | No manual range logic. |
| 17 | fixed | The owned admission permit serializes only request budget state and is not held across unrelated mutex guards. |
| 18 | checked | Tests remain in sibling modules; no duplicated or production-wide lint allowance. |
| 19 | checked | `cargo fmt --all -- --check` passes. |
| 20 | fixed | Test baseline updated to and verified at 4254; no test was removed or ignored. |
| 21 | checked | Every suite used the required 8 MiB test stack. |
| 22 | checked | No process-global subscriber/state installation. |
| 23 | checked | No migration/schema bootstrap change. |
| 24 | checked | Unknown custom-model pricing fails closed only when a run cost cap exists; catalog authority is unchanged. |
| 25 | checked | No render benchmark. |
| 26 | checked | No platform filesystem behavior change. |
| 27 | checked | No Windows wire behavior change. |
| 28 | checked | No process-tree runner change. |
| 29 | checked | No autospawn authorization change. |
| 30 | fixed | Budget tests fail on every wrong terminal and use one derived five-second case deadline. |
| 31 | checked | No Android change. |
| 32 | checked | No release action. |
| 33 | checked | No test-runner policy change. |
| 34 | checked | No dependency module or Cargo feature introduced. |
| 35 | checked | No trait-method ambiguity. |
| 36 | checked | No temporary reference is borrowed through `?`. |
| 37 | checked | No cfg-boundary type changed. |
| 38 | checked | Budget maps/sets are queried with their declared key types. |
| 39 | checked | Every changed test source compiles in its full crate suite. |
| 40 | checked | No cfg-Windows dependency error conversion. |
| 41 | checked | No endpoint basename or bind path change. |
| 42 | checked | No cold-launch timing assertion. |
| 43 | checked | No descriptor sweep change. |
| 44 | checked | Required local fake-provider and real daemon/CLI tests executed in this environment. |
| 45 | checked | Unsafe-count guard passes; no unsafe block added. |
| 46 | checked | No runtime-root derivation change. |
| 47 | checked | No filesystem walker. |
| 48 | fixed | New protocol tests use the declared sibling `headless_tests.rs` module form. |
| 49 | checked | No queued-batch acknowledgement logic change. |
| 50 | checked | No platform-dependent serialized byte pin. |
| 51 | checked | No profile-lock change. |
| 52 | checked | No help viewport change. |
| 53 | checked | No runtime-root permission change. |
| 54 | checked | Required runner stack was present and every affected later binary ran. |
| 55 | checked | No cfg-Windows unit-valued binding. |
| 56 | fixed | Budget reason maps to exit 77 in every phase; timeout remains the distinct exit 124. |
| 57 | checked | No UI layout pin. |
| 58 | checked | No result-storage threshold change. |
| 59 | checked | No roster row change. |
| 60 | checked | No Windows connection-liveness change. |
| 61 | fixed | Every claimed budget guarantee has a named test or the brace-bounded transport-order mutation pin, including native PDF bytes and repeated references. |
| 62 | checked | The provider guard is additive; no existing public return type changed. |
| 63 | checked | No platform archive tool. |
| 64 | checked | Fresh binaries are valid Mach-O; `haiderd` is 181,298,496 bytes. |
| 65 | checked | Tests assert typed reasons, not raw platform errnos. |
| 66 | checked | No STT surface. |
| 67 | checked | Both sibling binaries were prebuilt and fixture suites used the prebuilt flag. |
| 68 | fixed | Missing post-send usage is distinguished from normal reconciliation and terminalizes `usage_unavailable`. |
| 69 | checked | No executable path construction. |
| 70 | checked | No workflow trigger/dispatch change. |
| 71 | fixed | Fake-provider tests observe zero/one exact request counts and the CLI test runs the built artifact to one typed terminal. |
| 72 | checked | Discovery-disabled is the mandated runner setting; this lane does not touch discovery. |
| 73 | fixed | The compaction mutation pin brace-matches the full impl instead of using a fixed byte window. |
| 74 | checked | Real-daemon fixtures retain their temporary machine-user home behavior. |
| 75 | checked | No hub actor shutdown/drain ownership change. |
| 76 | fixed | Additive budget decision fields are projected through client and CLI tests rather than silently dropped. |
| 77 | checked | Unsafe counts were the first final repository guard and passed before the remaining checks. |
| 78 | checked | No release/tag dispatch. |
| 94 | fixed | Action cases document two sequential bounds: 1,000 ms request start + (4,000 ms reconciliation + one 10 ms poll) = 5,010 ms total. |
| 95 | checked | No new external-state wait holds a negotiated connection; fake-provider waits are in-process and continuously bounded. |

No new CI error class was discovered by this lane.
