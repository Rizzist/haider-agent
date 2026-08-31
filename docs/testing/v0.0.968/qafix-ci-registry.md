# QA gate adjudication fixes — verification and CI error registry audit

## Result

- `scripts/qa-gate/run.sh test`: PASS, 24/24.
- `cargo test -p haider-rpc --locked --test automation_contract_doc_tests`:
  PASS, 1/1, with the mandated CI environment and 27 GiB free before build.
- installed `/usr/local/bin` v0.0.967 T0 run: 6 PASS, 2 expected truthful
  budget FAIL, 1 expected-gap SKIP. In particular, `alias_selects` and
  `wait_ready_n` pass; `exit_codes` records SIGINT as SKIP with
  `observed_exit=-2`; only max-cost and max-tokens remain FAIL on 967.
- final report:
  `/private/tmp/qafix-t0-report-20260831-r5/qa-gate-t0-Syeds-MacBook-Air.local-20260831T174327Z.json`.
- Python compile, locked Cargo metadata, `git diff --check`, conflict-marker
  scan, and unmerged-index scan: PASS.
- installed binaries are arm64 Mach-O files (33 MiB `haider`, 49 MiB
  `haiderd`); the daemon exceeds the 10 MiB truncation sentinel.

## Citation audit

| Brief citation | Verdict | Current evidence |
| --- | --- | --- |
| `docs/client-contract-v1.md:2931` | drifted | The no-descriptor statement is now `:2933-2935`; `:2931` ends the preceding probe-reference sentence. |
| `crates/haider-cli/src/account.rs:731/:746/:832` | correct | `:731-735` selects keyed/no-auth mode, `:746-765` sends `ProviderConfigure`, and `:832-845` enters `AccountLoginApi` only for a staged key. |
| `crates/haider-daemon/src/session_hub/rpc.rs:14907` | correct | Descriptor-only alias resolution is `:14907-14940`, including `account_not_found`. |
| readiness in `automation.rs` / client | check citation corrected | The CLI current-format predicate is `crates/haider-cli/src/automation.rs:251-260`; the semantic definition and predicate are `crates/haider-client/src/observe.rs:121-126`, `:856-858`, and `:1184-1205`. |
| `crates/haider-client/src/headless.rs:3040` | correct | The select at `:3040-3135` has frame and deadline branches, with no signal branch. |
| `crates/haider-cli/src/run.rs:776` | correct | `:776-784` directly awaits the headless run and installs no SIGINT handler. |

## §A–§D registry walk

`checked: none` means the class was inspected and this Python/docs lane
introduces no instance. `fixed` names the lane location that directly exercises
the registry law.

| Class | Result | Evidence |
| ---: | --- | --- |
| 1 | checked: none | No Rust struct, enum, constructor, or match changed. |
| 2 | checked: none | No Rust API or signature changed. |
| 3 | checked: none | No Rust ownership-bearing code changed. |
| 4 | checked: none | No Rust visibility seam changed. |
| 5 | checked: none | No cfg-narrow import was added. |
| 6 | checked: none | No enum/import table changed. |
| 7 | checked: none | No manifest or lockfile edit; locked metadata passes. |
| 8 | checked: none | No mechanical source sweep was used. |
| 9 | checked: none | No Rust Clippy surface changed. |
| 10 | checked: none | Every added Python helper/value is exercised by the named T0 checks. |
| 11 | checked: none | No Rust combinator/style surface changed. |
| 12 | checked: none | No Rust function signature changed. |
| 13 | checked: none | No Rust type-complexity surface changed. |
| 14 | checked: none | No Rust derive changed. |
| 15 | checked: none | No iterator-last rewrite. |
| 16 | checked: none | No Rust range expression changed. |
| 17 | checked: none | No Rust async lock changed. |
| 18 | checked: none | No lint attribute or production unwrap/expect changed. |
| 19 | fixed: `scripts/qa-gate/checks/t0/t0.sessions.wait_ready_n.py:1` | All changed Python compiles and `git diff --check` passes; no `.rs` changed. |
| 20 | checked: none | No Rust test was added; the Python budget pin was updated in place. |
| 21 | checked: none | The Rust doc test used `RUST_MIN_STACK=8388608`. |
| 22 | checked: none | No process-global state is installed. |
| 23 | checked: none | No migration/schema changed. |
| 24 | checked: none | Provider catalog authority is untouched. |
| 25 | checked: none | No render benchmark changed. |
| 26 | checked: none | No production filesystem/platform code changed. |
| 27 | checked: none | Windows wire behavior is unchanged. |
| 28 | checked: none | No process-test runner changed. |
| 29 | checked: none | Autospawn behavior is untouched. |
| 30 | fixed: `scripts/qa-gate/checks/t0/t0.sessions.wait_ready_n.py:196` | Finite segments are settled deterministically and every unexpected state reports its actual value. |
| 31 | checked: none | Android is untouched. |
| 32 | checked: none | No release action was taken. |
| 33 | checked: none | Runner behavior is unchanged; only check logic and its budget pin changed. |
| 34 | checked: none | No dependency or feature was added. |
| 35 | checked: none | No Rust trait call changed. |
| 36 | checked: none | No Rust temporary borrow changed. |
| 37 | checked: none | No cfg-boundary type changed. |
| 38 | checked: none | No collection-key seam changed. |
| 39 | checked: none | No new Rust test file or private API call was added. |
| 40 | checked: none | No dependency error conversion changed. |
| 41 | checked: none | No UDS path construction changed; the installed check used the existing short-root harness. |
| 42 | checked: none | No cold-binary timing assertion was added. |
| 43 | checked: none | No descriptor sweep changed. |
| 44 | fixed: installed T0 report | Real installed-binary UDS and loopback checks passed in this managed lane. |
| 45 | checked: none | No unsafe code was added. |
| 46 | checked: none | Runtime-root derivation is untouched. |
| 47 | checked: none | No filesystem walker was added. |
| 48 | checked: none | No Rust source test file was added. |
| 49 | checked: none | No queued acknowledgement path changed. |
| 50 | checked: none | No platform-dependent byte pin changed. |
| 51 | checked: none | Profile-lock behavior is untouched. |
| 52 | checked: none | TUI viewport behavior is untouched. |
| 53 | checked: none | Runtime-root permission behavior is untouched. |
| 54 | checked: none | The only Rust suite used the required stack and completed. |
| 55 | checked: none | No cfg-Windows value binding changed. |
| 56 | checked: none | Product deadline reason/exit mapping is untouched. |
| 57 | checked: none | No UI layout pin changed. |
| 58 | checked: none | CAS thresholds are untouched. |
| 59 | checked: none | Roster row grammar is untouched. |
| 60 | checked: none | Windows connection liveness is untouched. |
| 61 | fixed: `scripts/qa-gate/checks/t0/t0.account.alias_selects.py:88` | The documented account-selection guarantee is exercised by the installed gate, including persistence. |
| 62 | checked: none | No public Rust return type changed. |
| 63 | checked: none | No external archive/platform utility is invoked. |
| 64 | fixed: installed binary inspection | `haiderd` is a valid 49 MiB Mach-O, above the 10 MiB sentinel. |
| 65 | checked: none | Assertions use typed semantic JSON, not raw errno. |
| 66 | checked: none | STT is untouched. |
| 67 | checked: none | No daemon/client Rust subprocess suite was run without prebuilt siblings. |
| 68 | checked: none | No swallowed product error was hardened. |
| 69 | checked: none | No executable-path casing logic changed. |
| 70 | checked: none | No workflow trigger or dispatch changed. |
| 71 | fixed: installed T0 report | The real installed 0.0.967 pair completed the full T0 smoke. |
| 72 | fixed: `scripts/qa-gate/checks/t0/t0.account.alias_selects.py:77` | Native discovery stays out of scope while explicit hermetic keys exercise real custom-provider credentials. |
| 73 | checked: none | No fixed-window source scan was added. |
| 74 | checked: none | The existing QA context supplies scratch HOME/USERPROFILE; no global state changed. |
| 75 | checked: none | Hub shutdown ownership is untouched. |
| 76 | fixed: `scripts/qa-gate/checks/t0/t0.account.alias_selects.py:170` | Persistence is read from the authoritative raw `SessionSummary.metadata`, not a CLI projection that omits the field. |
| 77 | checked: none | Applicable locked-metadata, syntax, diff, conflict, and doc-example guards pass. |
| 78 | checked: none | No tag or release dispatch occurred. |
| 79 | checked: none | Process completion ownership is untouched. |
| 80 | fixed: `scripts/qa-gate/checks/t0/t0.sessions.wait_ready_n.py:199` | Bounded resume settles two turns only to construct state; readiness is asserted separately and does not imply idle. |
| 81 | checked: none | Process output readers are unchanged. |
| 82 | checked: none | Foreground/background process ownership is untouched. |
| 83 | checked: none | Detach-failure behavior is untouched. |
| 84 | checked: none | No paused-time process test was added. |
| 85 | checked: none | Product terminal classification is untouched. |
| 86 | checked: none | Process exit observation is unchanged and existing cleanup passed. |
| 87 | checked: none | No thread-count lifecycle fence changed. |
| 88 | checked: none | No staging-file publication path changed. |
| 89 | checked: none | No Windows endpoint-parent assertion changed. |
| 90 | checked: none | No sparse-file fixture changed. |
| 91 | fixed: `scripts/qa-gate/checks/t0/t0.run.exit_codes.py:305` | Expected-gap evidence is a single machine-readable line with actual values. |
| 92 | checked: none | No maintenance-loop counter/timer changed. |
| 93 | checked: none | No subprocess throughput sampler changed. |
| 94 | fixed: `scripts/qa-gate/checks/t0/t0.sessions.wait_ready_n.py:46` | Both changed outer budgets sum every nested start, resume, readiness, and cleanup bound; their pins are 253,000 ms and 520,000 ms. |
| 95 | checked: none | Each Python operation is a finite CLI one-shot; no external-state wait holds a negotiated connection open. |

No new CI error class was discovered by this lane.
