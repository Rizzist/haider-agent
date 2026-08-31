# CI-PREP — lane 968-contract

## Result

The brief's `$T/ci-prep.sh` is unavailable (`T` is unset), so its applicable
lane-scoped checks were run directly. The mission limits builds to
`haider-rpc` and `haider-protocol`, so no workspace-wide check or Clippy run
was substituted for the required targeted verification.

- free space before every Cargo build: PASS (more than 32 GiB available)
- unsafe-count guard: PASS (`production=188`, `test=15`)
- locked metadata: PASS; this lane changes no manifest or lockfile
- Rust formatting, `git diff --check`, conflict-marker and unmerged-index
  checks: PASS
- affected-package deny-warning Clippy: PASS (`haider-rpc`,
  `haider-protocol`, all targets, locked)
- complete affected-package suites: PASS
- named schema-changelog pin and automation-doc example tests: PASS
- changelog-marker mutation: PASS; removing `payload:harness_status` produced
  the expected named-test failure and the marker was restored
- test ledger: updated from 4239 to 4241
- wire goldens: unchanged

## Citation audit

- `crates/haider-protocol/src/envelope.rs:9`: **partly correct**. The current
  line requires a version bump, upcaster, and old/new goldens for “schema
  changes”; the owner-approved contract narrows a bump to non-additive changes.
- `crates/haider-rpc/src/uds_codec.rs`: **correct** for the four-byte prefix,
  negotiated JSON/MessagePack body, frame limit, and poison behavior; the
  operative ranges are `:1-22`, `:727-751`, and `:858-940`.
- `crates/haider-rpc/src/negotiation.rs`: **correct**; range negotiation is at
  `:32-45` and `:65-100`.
- `crates/haider-rpc/src/frame.rs`: **wrong as phrased in the brief**. The enum
  has more than six variants plus `Unknown`; the guide names the six
  foundational handshake/request/event variants without calling the enum
  exhaustive (`:5310-5348`).
- `crates/haider-rpc/tests/fixtures/client_contract_methods_v1.json` and
  `wire_transcript.json`: **correct** and used byte-for-byte by the examples.
- `docs/jsonl-run-contract-v1.md`: **correct** for terminal kinds and carrier
  shape at `:7-22` and `:61-102`; usage is a preceding envelope rather than a
  field of the terminal.
- `crates/haider-tools/src/spawn_subagent.rs`: **correct**; the typed arguments
  and bounds are at `:8-46`, validation is at `:48-96`, and the provider-facing
  JSON schema is at `:115-179`.
- `docs/client-contract-v1.md`: **correct for its published N-1 baseline**
  (`0.0.964` at `:3-6`), whose same header names source package `0.0.965`.
- `lane-968-rtdir`: **visible, unmerged sibling change**. Its field is
  `runtime_dir_resolution`, a CLI status projection rather than an RPC field;
  the guide marks it as landing in 968.

## §A–§D registry walk

`checked: none` means the class was inspected and this lane introduces no
instance. `fixed` names the lane location that directly addresses the class.

| Class | Result | Evidence |
|---:|---|---|
| 1 | checked: none | No production struct, enum, constructor, or match changed. |
| 2 | checked: none | No production API or signature changed. |
| 3 | checked: none | No ownership-bearing production code changed. |
| 4 | checked: none | Tests use only public frame and protocol types. |
| 5 | checked: none | No platform-gated import was added. |
| 6 | fixed: `crates/haider-protocol/tests/schema_changelog_tests.rs:8` | One macro table generates each kind inventory and exhaustive match. |
| 7 | checked: none | No manifest/lock edit; locked metadata passes. |
| 8 | checked: none | No mechanical source sweep was used. |
| 9 | checked: none | Targeted deny-warning Clippy is clean. |
| 10 | fixed: `crates/haider-protocol/tests/schema_changelog_tests.rs:8` | The two intentionally compile-only exhaustive match functions are macro-generated mutation pins; only those carry a scoped `dead_code` allowance. |
| 11 | checked: none | Targeted deny-warning Clippy is clean. |
| 12 | checked: none | No over-wide signature finding. |
| 13 | checked: none | No type-complexity finding. |
| 14 | checked: none | No derive seam changed. |
| 15 | checked: none | No iterator rewrite. |
| 16 | checked: none | No range expression changed. |
| 17 | checked: none | No async lock-bearing code changed. |
| 18 | fixed: `crates/haider-protocol/tests/schema_changelog_tests.rs:1` | Test-only `expect` lint allowance is inner and singular. |
| 19 | checked: none | Rustfmt and `git diff --check` pass. |
| 20 | fixed: `test-baseline.txt:1` | Test floor advanced by the two new integration tests. |
| 21 | checked: none | Every Cargo test used the mandated 8 MiB stack. |
| 22 | checked: none | No process-global state is installed. |
| 23 | checked: none | No schema migration changed. |
| 24 | checked: none | Provider catalogs are untouched. |
| 25 | checked: none | No render benchmark changed. |
| 26 | checked: none | No filesystem platform code changed. |
| 27 | checked: none | Windows wire behavior is unchanged. |
| 28 | checked: none | No process-test runner changed. |
| 29 | checked: none | Autospawn behavior is untouched. |
| 30 | checked: none | No external signal wait or fake-provider script was added. |
| 31 | checked: none | Android is untouched. |
| 32 | checked: none | No release action was taken. |
| 33 | checked: none | No runner behavior changed. |
| 34 | checked: none | No dependency module or feature was added. |
| 35 | checked: none | No trait-method ambiguity. |
| 36 | checked: none | No temporary is borrowed through `?`. |
| 37 | checked: none | No cfg-boundary type changed. |
| 38 | checked: none | No collection-key code changed. |
| 39 | fixed: `crates/haider-rpc/tests/automation_contract_doc_tests.rs:1` | Both new integration-test files compile under all-target Clippy and pass. |
| 40 | checked: none | No Windows dependency error conversion. |
| 41 | checked: none | No UDS path is constructed or bound. |
| 42 | checked: none | No timing assertion or binary launch. |
| 43 | checked: none | No descriptor sweep changed. |
| 44 | checked: none | This lane claims no socket-bind runtime proof. |
| 45 | checked: none | No unsafe block added; unsafe-count guard passes. |
| 46 | checked: none | Runtime-root validation is untouched. |
| 47 | checked: none | No filesystem walker was added. |
| 48 | checked: none | New tests are Cargo integration-test files, not sibling source tests. |
| 49 | checked: none | No queued acknowledgement path changed. |
| 50 | checked: none | No platform-dependent size pin. |
| 51 | checked: none | Profile-lock behavior is untouched. |
| 52 | checked: none | TUI viewport is untouched. |
| 53 | checked: none | Runtime-root permissions are untouched. |
| 54 | checked: none | Required runner environment was used; both suites completed. |
| 55 | checked: none | No cfg-Windows value binding. |
| 56 | checked: none | No deadline or exit mapping changed. |
| 57 | checked: none | No UI layout pin changed. |
| 58 | checked: none | CAS thresholds are untouched. |
| 59 | checked: none | Roster grammar is untouched. |
| 60 | checked: none | Windows connection liveness is untouched. |
| 61 | fixed: `crates/haider-rpc/tests/automation_contract_doc_tests.rs:96` | Required catalog coverage, exact inventory, correlation, real-type decoding, and golden membership are asserted. |
| 62 | checked: none | No public return type changed. |
| 63 | checked: none | No external platform utility is invoked. |
| 64 | checked: none | No executable was linked or exercised by this docs/test-only lane. |
| 65 | checked: none | No errno mapping changed. |
| 66 | checked: none | STT is untouched. |
| 67 | checked: none | No daemon/client subprocess suite is in scope. |
| 68 | checked: none | No error handling changed. |
| 69 | checked: none | No executable path is constructed. |
| 70 | checked: none | No CI trigger or dispatch changed. |
| 71 | checked: none | No release artifact is produced or promoted. |
| 72 | checked: none | Credential discovery is untouched. |
| 73 | fixed: `crates/haider-protocol/tests/schema_changelog_tests.rs:99` | Terminal source parsing is delimiter-based, not a fixed byte window. |
| 74 | checked: none | No machine-user-global subsystem changed. |
| 75 | checked: none | Hub shutdown ownership is untouched. |
| 76 | fixed: `docs/event-schema-changelog.md:1` | Additive event changes and consumer obligations are explicit and pinned. |
| 77 | checked: none | Repository unsafe guard passes before package tests. |
| 78 | checked: none | No tag or release dispatch. |
| 79 | checked: none | Process completion ownership is untouched. |
| 80 | checked: none | No run-terminal/session-idle wait was added. |
| 81 | checked: none | Process output readers are untouched. |
| 82 | checked: none | Foreground/background process ownership is untouched. |
| 83 | checked: none | Detach-failure behavior is untouched. |
| 84 | checked: none | No paused-time process test. |
| 85 | checked: none | Process terminal classification is untouched. |
| 86 | checked: none | Process exit observation is untouched. |
| 87 | checked: none | No thread-count lifecycle fence. |
| 88 | checked: none | No staging-file publication path. |
| 89 | checked: none | No Windows endpoint-parent assertion. |
| 90 | checked: none | No sparse-file fixture. |
| 91 | checked: none | No line-ending-sensitive source pin. |
| 92 | checked: none | No maintenance-loop counter or timer. |
| 93 | checked: none | No subprocess sampling assertion. |
| 94 | checked: none | No deadline was added. |
| 95 | checked: none | No external-state wait exists on a negotiated connection. |

No new CI error class was discovered by this lane.
