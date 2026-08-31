# CI-PREP — lane 968-rtdir

## Result

The registry's `$T/ci-prep.sh` is unavailable because `T` is unset, so the
applicable focused checks were run directly. The lane brief forbids
workspace-wide Clippy; both affected crates were checked together with
`--all-targets --locked -- -D warnings`.

- unsafe-count guard: PASS (`production=188`, `test=15`)
- locked metadata: PASS
- dependency duplicates: reviewed; this lane changes no manifest or lockfile
- targeted Rust formatting, `git diff --check`, conflict-marker and unmerged
  index checks: PASS
- additive status fixture JSON: parsed by the exact golden test
- affected-package check and deny-warning Clippy: PASS (`haider-client`,
  `haider-cli`)
- complete affected suites: PASS, including four real-binary status tests
- test ledger: deliberately updated from 4239 to 4244
- final binaries: arm64 Mach-O; `haider=102377744` bytes and
  `haiderd=180825872` bytes; the daemon exceeds 10 MiB

## Citation audit

Every citation in the brief was **correct on the starting tree** at `424f2ca`:
the module contract was `profile.rs:13-19`, the typed remedy was line 178, the
endpoint fallback was lines 305-329, XDG acceptance was line 370, and
`verified_owner_private` was line 415. The implementation deliberately moved
those locations: the contract is now lines 12-24, the typed remedy line 276,
the explicit-fail/derived-fallback split lines 421-453, XDG selection lines
514-530, and `verified_owner_private` lines 629-633. The brief's description of
`<root>/<hex>/h.sock` was directionally correct, with one important nuance:
validation budgets the longer staging pathname, so the typed error's actual
`length` and `limit` are authoritative. The observe schema remains at line 23
(formerly line 22); its additive resolution projection is lines 67-77,
240-272, and 549-614.

## §A–§D registry walk

`checked: none` means the class was inspected and this lane introduces no
instance. `fixed` names the lane location that exercises or reconciles it.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed: `crates/haider-client/src/profile.rs:133` | New detailed types use a companion resolver, leaving all `ResolvedProfile` literals source-compatible. |
| 2 | checked: none | Existing resolver signatures remain; the new function is additive. |
| 3 | checked: none | Check and Clippy found no moved-after-use value. |
| 4 | checked: none | No private field was exposed to integration tests. |
| 5 | fixed: `crates/haider-client/src/profile.rs:590` | Unix metadata imports and helper are cfg-scoped. |
| 6 | checked: none | No duplicate import or enum variant. |
| 7 | checked: none | No manifest/lock edit; locked metadata passes. |
| 8 | checked: none | No mechanical sweep was used. |
| 9 | checked: none | Targeted deny-warning Clippy is clean. |
| 10 | checked: none | No dead or unused helper remains. |
| 11 | checked: none | Targeted deny-warning Clippy is clean. |
| 12 | checked: none | No over-wide production signature finding. |
| 13 | checked: none | No type-complexity finding. |
| 14 | fixed: `crates/haider-client/src/profile.rs:134` | Additive resolution types derive both `PartialEq` and `Eq`. |
| 15 | checked: none | No iterator-last rewrite. |
| 16 | checked: none | No manual-range finding. |
| 17 | checked: none | No lock is held across an await. |
| 18 | checked: none | No duplicate lint attribute or production unwrap/expect. |
| 19 | checked: none | Targeted rustfmt and diff check pass. |
| 20 | fixed: `test-baseline.txt:1` | Five named tests moved the ledger from 4239 to 4244. |
| 21 | checked: none | Every test command used the mandated 8 MiB stack. |
| 22 | checked: none | No tracing subscriber installation. |
| 23 | checked: none | No migration. The observe schema change is additive and goldened. |
| 24 | checked: none | Provider catalog authority is untouched. |
| 25 | checked: none | No rendering benchmark. |
| 26 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:554` | One cross-platform test asserts Unix failure and Windows named-pipe success by inspection/compilation. |
| 27 | checked: none | Windows wire semantics and endpoint construction are unchanged. |
| 28 | checked: none | No process-test runner change. |
| 29 | checked: none | Autospawn authorization is untouched; the complete CLI suite passes. |
| 30 | checked: none | No terminal signal observer was added. |
| 31 | checked: none | Android is untouched. |
| 32 | checked: none | No release action. |
| 33 | checked: none | No CI runner behavior change. |
| 34 | checked: none | No dependency module or feature introduced. |
| 35 | checked: none | No trait-method ambiguity. |
| 36 | checked: none | No temporary is borrowed through `?`. |
| 37 | checked: none | Windows cfg arms retain path/profile types. |
| 38 | checked: none | No collection key mismatch. |
| 39 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:431` | Changed integration tests compile under all-target Clippy and pass as real binaries. |
| 40 | checked: none | No Windows dependency error conversion. |
| 41 | fixed: `crates/haider-client/src/profile.rs:421` | Every endpoint is budget-checked; explicit overflow is typed and derived overflow is reported. |
| 42 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:70` | Both binaries are warmed once before timed launches. |
| 43 | checked: none | Descriptor sweeping is untouched. |
| 44 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:124` | Real UDS status tests passed in the managed lane; other kernels remain CI-owned. |
| 45 | checked: none | Unsafe-count guard passes; no unsafe block was added. |
| 46 | checked: none | Sticky-root owner scoping is preserved. |
| 47 | checked: none | No filesystem walker. |
| 48 | checked: none | Tests were added to existing declared sibling/integration test files. |
| 49 | checked: none | No queued acknowledgement path. |
| 50 | checked: none | No cfg-dependent exact byte-size pin. |
| 51 | checked: none | Profile-lock cleanup is untouched. |
| 52 | checked: none | TUI layout is untouched. |
| 53 | checked: none | Runtime preparation permissions remain owned by `haider-platform`. |
| 54 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:431` | Correct runner environment was used and every affected binary completed. |
| 55 | checked: none | No cfg-Windows unit-valued binding. |
| 56 | checked: none | Deadline-reason mapping is untouched. |
| 57 | checked: none | No UI pin changed. |
| 58 | checked: none | CAS thresholds are untouched. |
| 59 | checked: none | Roster grammar is untouched. |
| 60 | checked: none | Windows connection liveness is untouched. |
| 61 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:554` | The explicit-isolation guarantee is enforced by a mutation-strength real-binary test. |
| 62 | checked: none | Existing public return types remain unchanged. |
| 63 | checked: none | No platform archive tool. |
| 64 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:62` | The daemon integrity floor is 10 MiB; built binaries were also inspected as Mach-O. |
| 65 | checked: none | No raw errno projection. |
| 66 | checked: none | STT is untouched. |
| 67 | fixed: verification command | `haider` and `haiderd` were prebuilt and `HAIDER_TEST_SIBLINGS_PREBUILT=1` was set. |
| 68 | checked: none | No swallowed error was hardened. |
| 69 | checked: none | No executable path casing logic. |
| 70 | checked: none | No CI trigger or dispatch change. |
| 71 | fixed: `crates/haider-cli/tests/status_discovery_smoke_tests.rs:431` | The shipped-style CLI/daemon pair was exercised end-to-end with bounded JSON parsing. |
| 72 | checked: none | Existing enabled-discovery smoke remains enabled and passes. |
| 73 | checked: none | No fixed-window source scan. |
| 74 | checked: none | Every new daemon subprocess uses an isolated machine-user home. |
| 75 | checked: none | Hub shutdown is untouched. |
| 76 | fixed: `crates/haider-cli/src/observe.rs:607` | The additive client resolution is explicitly projected into status JSON and goldened. |
| 77 | fixed: repository guards | Unsafe counts and locked metadata ran before final review. |
| 78 | checked: none | No tag/release dispatch. |
| 79 | checked: none | Process completion ownership is untouched. |
| 80 | checked: none | No run-terminal/session-idle observer change. |
| 81 | checked: none | Process output readers are untouched. |
| 82 | checked: none | Foreground/background process ownership is untouched. |
| 83 | checked: none | Detach-failure behavior is untouched. |
| 84 | checked: none | No paused-time real-process test. |
| 85 | checked: none | Process terminal classification is untouched. |
| 86 | checked: none | Process exit observers are untouched. |
| 87 | checked: none | No thread-count lifecycle fence. |
| 88 | checked: none | Staging-file publication is untouched. |
| 89 | checked: none | Windows endpoint parent semantics are unchanged. |
| 90 | checked: none | No sparse-file fixture. |
| 91 | checked: none | No line-ending-sensitive source pin. |
| 92 | checked: none | No maintenance-loop counter/timer. |
| 93 | checked: none | No subprocess sampling throughput assertion. |
| 94 | checked: none | No deadline was added; new cases reuse the established 20-second status bound. |
| 95 | checked: none | No external-state wait is left on an open negotiated connection. |

No new CI error class was discovered by this lane.
