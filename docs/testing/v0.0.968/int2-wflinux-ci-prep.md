# CI-PREP — lane 968-int2 / wflinux integration

## Result

Git metadata is read-only in this worktree: `git merge --no-ff
lane-968-wflinux` could not create `ORIG_HEAD.lock`. The resolved two-parent
merge result is therefore left unstaged in the working tree for the
orchestrator to commit. There are no unmerged index entries, conflict markers,
or reject files.

- unsafe-count guard: PASS (`production=188`, `test=16`)
- formatting and `git diff --check`: PASS
- test ledger recount: PASS, unchanged at 4263
- `haider-daemon --lib`: PASS (878 passed, 3 pre-existing live-test ignores)
- deterministic recovery test: PASS 20/20 with `--test-threads=1` and PASS
  20/20 with `--test-threads=8`
- delegation answer/continue, unanswered timeout/reap, cancellation-tail bound,
  and restart recovery: PASS independently
- `core_loop_e2e_tests`: PASS 14/14
- `haider-daemon` all-target Clippy with `-D warnings`: PASS
- mutation `max(latest_progress, nudge) -> nudge`: expected FAIL at
  `post-nudge progress averts the cancel`; restored test PASS
- final binaries: arm64 Mach-O; `haiderd=181,735,744` bytes (>10 MiB)

## Integration and citation audit

The brief's commit identities are correct: integration head `32501ab`, incoming
commit `9b049ef`, and merge base `424f2ca`. The described conflict constructs
are correct, but their implicit locations drifted. The resolved stall loop is
at `crates/haider-daemon/src/delegation.rs:1047`, the absolute wait budget at
`:1157`, `started.min(...)` at `:1213`, and the pure deadline helper at `:3069`.
The named test drifted from its pre-integration location to
`crates/haider-daemon/src/subagent_core_tests.rs:3582`.

Deleg's wait body, one absolute run-derived budget, typed timeout, durable
handoffs, terminal/reap tail, and descendant progress record remain intact.
Only the two stall predicates route through the injected seam, and cancellation
still anchors to `max(latest_progress, nudge)`. The clock imports, field, type,
constructor, and branch are all `cfg(all(test, unix))`. In a non-test build the
wrapper falls directly through to the original wall-clock predicate, whose
exact saturated-u64 expression was extracted unchanged into
`deadline_elapsed_at`. Production behavior is therefore byte-for-byte deleg's;
no production wait, select, deadline, handoff, or cancellation semantics changed.

## §A–§D registry walk

`checked: none` means the class was inspected and this integration introduces
no instance. `fixed` names the location that reconciles an applicable seam.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed: `crates/haider-daemon/src/delegation.rs:249` | Every `DelegationHandle` constructor initializes the cfg-only clock while retaining `run_wait_timeout`. |
| 2 | fixed: `crates/haider-daemon/src/delegation.rs:319` | The single stall-deadline seam and both callers compile. |
| 3 | checked: none | No moved-after-use diagnostic. |
| 4 | checked: none | No integration-test private-field access. |
| 5 | fixed: `crates/haider-daemon/src/delegation.rs:58` | Clock imports/state are cfg(test+unix); Windows and production shapes remain clean by inspection. |
| 6 | checked: none | No duplicate import or enum variant. |
| 7 | checked: none | No manifest or lockfile edit; locked Cargo commands pass. |
| 8 | fixed: `crates/haider-daemon/src/delegation.rs:1047` | The two overlapping merge hunks were reconciled once; no marker/reject remains. |
| 9 | checked: none | Deny-warning Clippy is clean. |
| 10 | checked: none | Every new helper is used; test-only helpers disappear outside tests. |
| 11 | checked: none | Deny-warning Clippy found no combinator/style issue. |
| 12 | checked: none | No over-wide production signature. |
| 13 | checked: none | No type-complexity finding. |
| 14 | checked: none | No incompatible derive seam. |
| 15 | checked: none | No iterator-last rewrite. |
| 16 | checked: none | No manual-range expression. |
| 17 | checked: none | Clock mutex guards are synchronous and never cross an await. |
| 18 | checked: none | No duplicate lint attribute or production unwrap/expect. |
| 19 | checked: none | `cargo fmt --all -- --check` and `git diff --check` pass. |
| 20 | checked: none | `xtask test-count --update` recount is unchanged at 4263. |
| 21 | checked: none | All Rust tests used `RUST_MIN_STACK=8388608`. |
| 22 | checked: none | No tracing subscriber installation. |
| 23 | checked: none | No migration or schema change. |
| 24 | checked: none | Provider catalog authority is untouched. |
| 25 | checked: none | No render benchmark. |
| 26 | checked: none | No production filesystem/platform seam changed. |
| 27 | checked: none | Windows wire semantics are untouched. |
| 28 | checked: none | No process-test runner change. |
| 29 | checked: none | Autospawn behavior is untouched. |
| 30 | fixed: `crates/haider-daemon/src/subagent_core_tests.rs:2005` | Every journal observation is bounded and timeout diagnostics include the last child journal. |
| 31 | checked: none | Android is untouched. |
| 32 | checked: none | No release action. |
| 33 | checked: none | No runner behavior change. |
| 34 | checked: none | No dependency feature/module introduced. |
| 35 | checked: none | No trait-method ambiguity. |
| 36 | checked: none | No temporary borrowed through `?`. |
| 37 | checked: none | cfg-boundary types remain consistent by inspection. |
| 38 | checked: none | No collection key mismatch. |
| 39 | fixed: `crates/haider-daemon/src/subagent_core_tests.rs:3582` | The changed sibling unit-test file compiles in the complete library suite and all-target Clippy. |
| 40 | checked: none | No Windows dependency-error conversion. |
| 41 | checked: none | No endpoint basename or UDS bind change. |
| 42 | checked: none | No cold-binary timing assertion. |
| 43 | checked: none | Descriptor sweeping is untouched. |
| 44 | checked: none | No new socket-binding proof claim. |
| 45 | checked: none | Unsafe-count guard passes; no unsafe block added. |
| 46 | checked: none | Runtime-root derivation is untouched. |
| 47 | checked: none | No filesystem walker. |
| 48 | checked: none | Test remains in the declared sibling module `subagent_core_tests.rs`. |
| 49 | checked: none | No queued acknowledgement path. |
| 50 | checked: none | No cfg-dependent serialized-size pin. |
| 51 | checked: none | Profile-lock cleanup is untouched. |
| 52 | checked: none | TUI viewport is untouched. |
| 53 | checked: none | Runtime-root permissions are untouched. |
| 54 | checked: none | CI runner environment was mirrored; complete binaries reached test completion. |
| 55 | checked: none | No cfg-Windows unit-valued binding. |
| 56 | checked: none | Deadline reason/exit mapping is untouched. |
| 57 | checked: none | No UI layout pin changed. |
| 58 | checked: none | CAS inline threshold is untouched. |
| 59 | checked: none | Roster grammar is untouched. |
| 60 | checked: none | Windows connection liveness is untouched. |
| 61 | fixed: `crates/haider-daemon/src/subagent_core_tests.rs:3803` | The asserted guarantee is mutation-proved, not comment-only. |
| 62 | checked: none | No public return type changed. |
| 63 | checked: none | No platform archive tool. |
| 64 | checked: none | Rebuilt `haiderd` is a 181,735,744-byte Mach-O. |
| 65 | checked: none | No raw errno projection. |
| 66 | checked: none | STT is untouched. |
| 67 | checked: none | CLI/daemon siblings were rebuilt and daemon tests used the prebuilt flag. |
| 68 | checked: none | No swallowed error was hardened. |
| 69 | checked: none | No executable-path casing logic. |
| 70 | checked: none | No CI trigger or dispatch change. |
| 71 | checked: none | Release artifact behavior is untouched. |
| 72 | checked: none | Credential discovery is untouched. |
| 73 | checked: none | No fixed-window source scan. |
| 74 | checked: none | No machine-user-global subsystem added. |
| 75 | checked: none | Hub shutdown ownership is untouched. |
| 76 | checked: none | No CLI wire projection field added. |
| 77 | checked: none | Repository unsafe-count guard passes before ship. |
| 78 | checked: none | No tag/release dispatch. |
| 79 | checked: none | Process completion ownership is untouched. |
| 80 | checked: none | No run-terminal/session-idle observer change. |
| 81 | checked: none | Process output readers are untouched. |
| 82 | checked: none | Foreground/background process ownership is untouched. |
| 83 | checked: none | Detach-failure behavior is untouched. |
| 84 | fixed: `crates/haider-daemon/src/delegation.rs:95` | Determinism uses one injected test clock, not paused Tokio time or a sleep. |
| 85 | checked: none | Process terminal classification is untouched. |
| 86 | checked: none | Process exit observers are untouched. |
| 87 | checked: none | No thread-count lifecycle fence. |
| 88 | checked: none | No staging-file publication change. |
| 89 | checked: none | Windows endpoint-parent semantics are unchanged. |
| 90 | checked: none | No sparse-file fixture. |
| 91 | checked: none | No line-ending-sensitive source pin. |
| 92 | fixed: `crates/haider-daemon/src/delegation.rs:120` | Clock checks are counted only after synchronous deadline evaluation, making the counter a phase fence. |
| 93 | checked: none | No subprocess sampling throughput assertion. |
| 94 | fixed: `crates/haider-daemon/src/subagent_core_tests.rs:3583` | Observation bound states `20 * (100ms + 100ms + 25ms + 25ms) = 5,000ms`. |
| 95 | checked: none | The deterministic test opens no negotiated transport while observing external state. |

No new CI error class was discovered by this integration.
