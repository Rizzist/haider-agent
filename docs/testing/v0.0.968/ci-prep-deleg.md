# CI-PREP — lane 968-deleg

## Citation audit

- Brief `delegation.rs:1793`: **wrong for the hang on this base**. It points
  into the durable stall-nudge path. The unbounded `InputRequired` wait was in
  `DelegationHandle::collect`: human-required children skipped stall action and
  the polling loop had no outer deadline.
- Brief `delegation.rs:1025`: **drifted but conceptually correct**. It named
  `cancel_ticket`; the detached terminal-mirror fallback was still present but
  its current line had moved.
- Brief `delegation.rs:2111`: **drifted but construct-correct**. It named
  `mirror_until_child_terminal`; its first terminal loop was unbounded while
  only the later metrics tail had a bound.
- Registry #94: **correct and directly applicable**. The worker already owns
  the canonical run deadline. Delegation now carries one absolute deadline and
  recomputes the remaining duration before active wait, cancellation, terminal
  mirror, and idle/metrics tail. Elapsed cancellation work never resets it.

## Direct gate result

The registry's `$T/ci-prep.sh` is not present in this worktree and no `$T`
value was supplied, so its applicable lane-scoped checks were run directly.
Every Cargo command used the required environment and was preceded by a disk
preflight well above 700 MiB. No manifest, lockfile, workflow, migration,
contract fixture, version, or test-count baseline changed.

- `bash scripts/check-unsafe-counts.sh`: PASS (`production=188`, `test=15`).
- `cargo metadata --locked`: PASS.
- Rust 2024 formatting check on every changed Rust file: PASS.
- `git diff --check`, unmerged-index and conflict-marker scans: PASS.
- `cargo tree -p haider-daemon -d --locked`: reviewed; dependency graph unchanged.
- `cargo check -p haider-daemon --tests --locked`: PASS.
- `cargo clippy -p haider-daemon --all-targets --locked -- -D warnings`: PASS.
- `cargo test -p haider-daemon --locked`: PASS, including the complete library,
  session-hub integration, smoke, state-machine, and doc-test binaries.
- `cargo run -p xtask -- test-count`: PASS (`4238` tests, baseline `4082`);
  no baseline edit required.
- Prebuilt siblings are valid arm64 Mach-O files; `haiderd` exceeds 10 MiB.

The final full-package run also exposed an existing load-sensitive observer in
`native_pipe_resume_skips_the_already_reconciled_batch`: its quiet-tick probe
could accept the prior sidecar while the next writer task was still queued.
The smallest out-of-lane, test-only correction drains the writer through the
existing `SessionHub::shutdown` contract before preserving the exact content
and duplicate-count assertions. The repaired full package run is green.

## Mutation proof

The active delegated-child `bounded_wait` was temporarily replaced with an
unbounded completion of the same future. Under that mutation,
`unanswered_delegated_question_times_out_parent_and_reaps_child` failed with
exit 101 after 5.55 seconds: its own diagnostic deadline observed only
`Queued`, `Thinking`, `Streaming`, `RunningTool`, `Streaming`, and
`Waiting(LocalChild)`, with no typed parent failure or terminal state. The
bounded implementation was restored immediately; the named test and complete
package then passed. This is the requested test-killing mutation, rather than
a source-presence assertion.

## §A–§D registry audit

| Class | Result |
|---:|---|
| 1 | fixed — new `RecoveredWork::DelegationMirror` was reconciled in every constructor and exhaustive match. |
| 2 | fixed — `collect`/`cancel_ticket` deadline signatures and all callers compile. |
| 3 | checked — package check found no moved-after-use value. |
| 4 | checked — tests use crate-visible behavior; no field was made public. |
| 5 | fixed — UDS-only imports/tests remain `cfg(unix)`; all-target Clippy is clean. |
| 6 | checked — no duplicate import or enum variant remains. |
| 7 | checked — no manifest/lock edit; locked metadata and package gates pass. |
| 8 | checked — edits were reread after each mutation/restoration; final diff is idempotent. |
| 9 | checked — deny-warnings Clippy found no collapsible control flow. |
| 10 | checked — no dead/unused helper remains. |
| 11 | checked — deny-warnings Clippy found no borrow/cast/combinator/default issue. |
| 12 | checked — no too-many-arguments diagnostic. |
| 13 | checked — no type-complexity diagnostic. |
| 14 | checked — journal handoff types derive compatible `Eq`/`PartialEq`. |
| 15 | checked — no iterator-last rewrite. |
| 16 | checked — no manual-range diagnostic. |
| 17 | checked — no lock guard crosses an await. |
| 18 | checked — no duplicate lint attribute or production unwrap/expect added. |
| 19 | checked — Rust 2024 formatting passes on every changed Rust file. |
| 20 | checked — test-count command reports the baseline valid; no blind bump. |
| 21 | checked — all tests used `RUST_MIN_STACK=8388608`. |
| 22 | checked — no process-global tracing subscriber installation. |
| 23 | checked — no schema or migration change. |
| 24 | checked — provider catalog authority untouched. |
| 25 | checked — no render benchmark claim. |
| 26 | checked — no platform filesystem behavior changed. |
| 27 | checked — Windows wire semantics untouched. |
| 28 | checked — no process-test runner change. |
| 29 | checked — autospawn policy untouched. |
| 30 | fixed — every new polling observer reports observed run/menu state; the load-sensitive native-pipe observer now waits on the writer's bounded drain rather than a quiet scheduler tick. |
| 31 | checked — Android untouched. |
| 32 | checked — no release rerun or tag action. |
| 33 | checked — required runner environment is scoped to invoked Cargo commands. |
| 34 | checked — no dependency module or feature introduced. |
| 35 | checked — no trait-method ambiguity. |
| 36 | checked — no temporary is borrowed through `?`. |
| 37 | checked — cfg seams compile under all-target package Clippy. |
| 38 | checked — no collection key-type mismatch. |
| 39 | fixed — every changed sibling test module compiles in the daemon package. |
| 40 | checked — no Windows dependency-error conversion. |
| 41 | checked — no endpoint path or UDS basename change. |
| 42 | checked — no cold-binary launch timing assertion. |
| 43 | checked — no descriptor sweep change. |
| 44 | checked locally — real paired-UDS workflow passes; other kernels remain CI-owned. |
| 45 | checked — unsafe-count guard passes and no unsafe block was added. |
| 46 | checked — runtime-root derivation untouched. |
| 47 | checked — no filesystem walker change. |
| 48 | checked — tests remain in the declared sibling `subagent_core_tests.rs` module. |
| 49 | fixed — receipt/projection event IDs are deterministic/idempotent, and the native-pipe queued batch is asserted only after the writer drain acknowledges it. |
| 50 | checked — no platform-dependent serialized byte pin. |
| 51 | checked — profile-lock behavior untouched. |
| 52 | checked — TUI help viewport untouched. |
| 53 | checked — runtime-root permissions untouched. |
| 54 | checked — complete suite used the CI 8 MiB stack and reached every binary. |
| 55 | checked — no cfg-Windows unit-valued binding. |
| 56 | fixed — delegated deadline failure is selected by typed reason and journals `ProviderTimeout`. |
| 57 | checked — no UI layout pin changed. |
| 58 | checked — CAS inline threshold untouched. |
| 59 | checked — roster grammar untouched. |
| 60 | checked — Windows connection-liveness seam untouched. |
| 61 | fixed — every claimed wait/cancel/recovery guarantee has a named behavioral assertion. |
| 62 | checked — no existing public return type changed. |
| 63 | checked — no platform archive tool. |
| 64 | checked — prebuilt `haiderd` is a valid Mach-O and exceeds 10 MiB. |
| 65 | checked — typed timeout/cancel outcomes do not expose raw errno. |
| 66 | checked — STT untouched. |
| 67 | checked — `haiderd`/`haider` were prebuilt and sibling-prebuilt flag was set. |
| 68 | checked — no swallowed error was hardened into an undifferentiated failure. |
| 69 | checked — no executable-path construction. |
| 70 | checked — workflow triggers untouched. |
| 71 | fixed — tests exercise real daemon session hub, UDS framing, journal, and recovery projections. |
| 72 | checked — credential discovery untouched; hermetic required environment used. |
| 73 | checked — no fixed-byte-window source scan. |
| 74 | checked — no machine-user-global subsystem or subprocess fixture change. |
| 75 | checked — hub drain ownership unchanged; nonblocking cancellation enqueue targets its actor. |
| 76 | checked — no additive wire field or CLI projection change. |
| 77 | checked — unsafe guard is included in the final direct gate. |
| 78 | checked — no tag/release dispatch. |
| 79 | checked — no durability-policy helper change. |
| 80 | fixed — tests distinguish child Run terminality from the following Session Idle reap fence. |
| 81 | checked — no process output-reader boundary change. |
| 82 | checked — no foreground/background process ownership change. |
| 83 | checked — no process detach fallback. |
| 84 | checked — no paused-time real-process test. |
| 85 | checked — cancellation classification remains typed and durable. |
| 86 | checked — no process exit-observer arm changed. |
| 87 | checked — accepted-run fencing untouched. |
| 88 | checked — no staged-file publication path. |
| 89 | checked — no Windows endpoint/path assertion. |
| 90 | checked — no sparse-file fixture. |
| 91 | checked — no source pin relies on line endings. |
| 92 | checked — no maintenance-loop counter/timer change. |
| 93 | checked — no group-commit timing bound changed. |
| 94 | fixed — active + reserved cancellation tail equals remaining run budget; every phase consumes one continuous absolute deadline with arithmetic documented in source/tests. |
| 95 | checked — no external-state wait is added while a negotiated connection is idle; the UDS test continues servicing frames during its bounded workflow. |

No new CI error class was exposed in this lane, so §D receives no new numbered
entry. The two verifier-found gaps were instances of the brief's existing
durable-handoff requirement and registry #94, not new CI classes.

## Integration addendum — lane 968-int1

The wfcont/deleg merge was re-audited against the current tree. The mission's
construct citations (`runtime.rs`, `turn_recovery.rs`, `worker.rs`,
`delegation.rs`, the session hub, and the six colliding test files) are all
correct; it supplied no numeric line citations to drift-check. The merged
startup reducer is now `startup-turn-recovery-v4`, forcing a full scan because
neither lane's older checkpoint cursor had reduced the other lane's facts.

The composite recovery order is pinned in source and by
`pending_cancellation_handoff_suppresses_workflow_continuation_shape`: pending
mirror replay/cancellation owns the run before a workflow continuation may
restore its logical request count and physical ordinal. Removing that guard
made the named test fail with exit 101; the correct guard was restored and the
test plus the full package passed.

Direct integrated gates (the registry's `$T/ci-prep.sh` was not present under
the available worktree roots) all passed:

- `scripts/check-unsafe-counts.sh`: production 188, test 15.
- locked metadata, Rust formatting, `git diff --check`, conflict-marker scan,
  and unmerged-index scan.
- `cargo test -p haider-daemon --lib`: 865 passed, 3 live-provider ignores.
- `cargo test -p haider-daemon --tests`: the same library result plus 103/103
  session-hub integration tests and both smoke/state-machine tests.
- `cargo test -p haider-core`: all unit/integration/doc-test binaries green;
  one pre-existing manual timing probe ignored.
- `cargo test -p haider-daemond --test core_loop_e2e_tests`: 14/14, including
  the four wfcont workflow cases and delegation cases.
- scoped all-target Clippy with `-D warnings` for `haider-core`,
  `haider-daemon`, and `haider-daemond`.
- test baseline recounted and verified at 4245.
- prebuilt `haiderd` is arm64 Mach-O, 181,224,512 bytes.

Integration registry walk: fixed classes #1/#2/#6/#20/#30/#39/#49/#56/#61/
#75/#76/#94/#95 are evidenced by the exhaustive field/match grep, composite
reducer/version/order, preserved tests, absolute deadline arithmetic, and
green scoped gates above. The verifier-found long-deadline mutation (capping
`explicit_deadline - now` by 241 seconds at each hop) is pinned by
`workflow_hops_do_not_refresh_the_delegation_wait_deadline`: both hops derive
the same durable `parent_start + 241s` bound. Classes #3-#5, #7-#19, #21-#29,
#31-#38, #40-#48,
#50-#55, #57-#60, #62-#74, and #77-#93 were checked with no new applicable
change or diagnostic. No new CI error class was found, so §D is unchanged.
