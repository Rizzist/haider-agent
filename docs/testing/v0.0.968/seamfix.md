# CI-PREP — lane 968-seamfix

## Provenance and pin transfer

- The worktree branch is `lane-968-seamfix`. Its committed `HEAD`,
  `b0fc75d2aa181086026c83a2c08e5c244df9959a`, exactly matches the current
  `origin/wave-968`. The common brief's older `8952219` base citation is
  therefore intentionally superseded by the orchestrator's merge-forward.
- Guard #77 ran before acceptance tests or product edits:
  `bash scripts/check-unsafe-counts.sh` passed with `production=188` and
  `test=16`.
- The acceptance tests from `lane-968-seam` at `c77f2fd` were ported first:
  359 inserted lines in `run_budget_tests.rs` and 945 insertions/8 deletions
  in `core_loop_e2e_tests.rs`. The current wave added
  `RunDeadlineExceeded`, so the port contains one compile-only exhaustive
  match arm beside `HeadlessRunConfigured`; no assertion, timeout, or expected
  outcome from the pin commit changed.
- Before the fixes, the three intended red pins reproduced independently:
  the cap-cleaned child terminalized `Failed`, the projected parent question
  answered `not_found` because it was no longer pending, and the admitted
  pre-response restart committed `Internal: run was interrupted by daemon
  restart` plus `Errored`. The two hold controls passed unchanged.

Git object writes are unavailable in this managed worktree: the worktree's
real `.git/worktrees/...` metadata is outside the writable root, and the first
`git cherry-pick c77f2fd` failed creating `index.lock` with `Operation not
permitted`. The source/test port was therefore applied without weakening it.
The common lane instructions also designate the orchestrator as commit owner.

## Citation audit

- Acceptance commit citation `event_store.rs:13313`: **drifted, construct
  correct**. The headless admission requirement is now
  `crates/haider-store/src/event_store.rs:13477`; it still requires the root
  session metadata to be `Autonomous`.
- Acceptance commit citation `interaction.rs:76`: **drifted by one, construct
  correct**. `crates/haider-protocol/src/interaction.rs:77` still resolves an
  autonomous, no-default `request_input` as `ReturnNoHumanAvailable`.
- Acceptance pin citations `core_loop_e2e_tests.rs:1983`, `:2900`, and
  `:3028`: **drifted by one only** after the current-wave exhaustive arm. The
  tests are now at `:1984`, `:2901`, and `:3029`.
- The hold citation `run_budget_tests.rs:1643`: **drifted** after the complete
  pin port; the retirement/spend law is now at `:1680`.
- The user note that the startup turn reducer was v5 was **correct on entry**.
  `RunReduction` gained response-evidence state, so the implementation and its
  single version pin now name `startup-turn-recovery-v6` at
  `turn_recovery.rs:100` and `:1896`.

## Product fixes

### Root-owned cap terminalization

Every participant still contributes usage to the shared root coordinator, but
an ordinary delegated child does not own the durable budget terminal. Budget
guard rejection is now typed as either terminal failure or cancellation from
pre-request admission through usage and final request release (including the
compaction-provider path), so a descendant cannot be flattened into a generic
store error at any of those seams. A descendant that crosses the shared cap
persists the root fact, then commits `Cancelled`; the child session returns to
`Idle`, and its spend is counted once. A directly submitted headless run keeps
its own terminal contract even if it reuses a delegated session and debits an
older durable root coordinator. This preserves the retirement/spend hold pin.

### One delegated-question admission contract

Headless root admission remains autonomous as required by the durable store.
Delegated child sessions are now explicitly interactive because their
`request_input` menu is durably projected to, and answered through, the parent.
Write/exec pre-allow remains independently bounded by the delegation grant.
The interaction mode is part of the child create receipt digest, so replay and
fresh creation cannot disagree. The former stale-policy unit was replaced by
a positive end-to-end UDS law that answers the projected parent menu and proves
the child never receives `no_human_available`.

### Pre-response admission recovery

Startup hydration now reduces provider-response evidence, including typed
provider items, usage, provider opaque extension facts, and route-replay
events. The narrow classifier retries only a first logical request that has a
durable admission marker, a compatible `Thinking`/`Streaming` state, and no
response, menu, budget, workflow deferral, partial item, or replay evidence.
It reconstructs request counts as consumed `0` and ordinal seed `0`, so the
physical retry retains logical ordinal 1 and the following request becomes 2.
The budget coordinator marks this exact handoff as a retry before preflight,
preventing the abandoned transport from becoming missing spend.

`RunReduction` changed, so checkpoint reducer v5 was bumped to v6 and the one
version pin was updated. Recovery-generated usage snapshots remain
request-local in the durable journal because no trustworthy process-local
cumulative baseline crosses the crash; the actor's internal accounting stays
cumulative. The acceptance journal therefore records only the two completed
1,000-token exchanges, not the abandoned attempt or a double-counted prefix.

The Unix process pin starts a detached fake-provider run, waits for durable
`Streaming`, sends a real `kill -9` to the daemon PID, then invokes
`session <id> recover --probe --json`. This path correctly has no
effect-ambiguity menu, so it returns typed `no_recovery` while observing
`run_state=running`, never `errored`; the original run subsequently reaches
`Done`. This is the same v6 admission-retry seam as the in-process restart pin.

## Verification

Every compiling Cargo command used the required 8 MiB
stack/discovery/device/incremental environment, every build/test/Clippy
command had a disk preflight above 700 MiB, and daemon subprocess suites used
freshly prebuilt siblings with `HAIDER_TEST_SIBLINGS_PREBUILT=1`.

- Imported acceptance and hold suite:
  `cargo test -p haider-daemond --test core_loop_e2e_tests --locked` —
  **18 passed**.
- Real process pin:
  `kill9_after_provider_admission_retries_instead_of_reporting_errored` —
  **passed** with an actual daemon `kill -9`.
- `cargo test -p haider-core --locked` — **passed**; one pre-existing manual
  timing probe ignored. Its 70-test runtime target includes the additional
  pre-request cancellation regression, which proves zero provider opens and a
  `Cancelled` terminal with no error.
- `cargo test -p haider-daemon --locked` — **902 passed, 3 pre-existing live
  provider tests ignored**, plus **103/103** session-hub integration tests and
  all smoke/state-machine/doc-test targets.
- `cargo test -p haider-daemond --locked` — **passed every unit and integration
  target**, including core loop 18/18 and lifecycle 37/37.
- `cargo test -p haider-cli --locked` — **passed every unit and integration
  target**, including CLI black-box 115/115.
- Scoped `cargo clippy` for `haider-core`, `haider-daemon`, `haider-daemond`,
  and `haider-cli`, all targets, locked, `-D warnings` — **passed**.
- `cargo fmt --all -- --check`, `git diff --check`, unmerged-index scan,
  conflict-marker scan, and locked metadata — **passed**.
- `bash scripts/check-unsafe-counts.sh` final rerun — **passed**
  (`production=188`, `test=16`).
- `cargo run -p xtask --locked -- check` — **passed**: 663 Rust files scanned,
  only the existing soft-cap warnings, and **4313 tests** against baseline
  **4305**. No baseline update was needed.
- Fresh siblings: `target/debug/haider` is 102,857,840 bytes and
  `target/debug/haiderd` is 184,402,816 bytes; the daemon exceeds registry
  #64's 10 MiB floor.

## CI error registry walk

| Registry class | Result |
|---|---|
| #1/#2/#6/#39 | Exhaustive recovered-work and payload matches were updated; all affected targets compile and test. |
| #5/#28/#37 | The real `kill -9` pin is correctly Unix-gated as requested; platform-neutral product code remains all-target Clippy-clean. Windows process execution is by inspection. |
| #7/#23/#34/#76 | No manifest, dependency, migration, public wire schema, or lockfile changed. Internal checkpoint shape changed with the required v6 invalidation. |
| #8/#9/#10/#11/#12/#13/#14/#15/#16/#17/#18/#19 | Final formatting, deny-warnings Clippy, diff checks, and complete package tests found no duplicate, dead, borrow, cast, complexity, lock-across-await, or lint regressions. |
| #20/#21/#54 | Test count is 4313 >= 4305; all Cargo tests used the required 8 MiB stack. |
| #30/#49/#61/#71 | Assertions are behavioral and journal/UDS/process backed: child terminal+idle, parent menu answer, restart ordinals/spend, and real daemon death/recovery. No source-presence-only proof is used. |
| #45/#77 | Unsafe guard ran before edits and after verification; both passed at production 188/test 16. |
| #64/#67 | CLI/daemon siblings were freshly prebuilt, the subprocess flag was set, and `haiderd` is 184,402,816 bytes. |
| #72/#74 | Discovery stayed disabled and subprocess profiles/homes are hermetic; no machine-user-global state is read or written. |
| #80/#85 | Child `Cancelled` terminal and following session `Idle` are distinct assertions; budget cancellation remains typed. |
| #82/#84/#86 | The new process test deliberately kills only the resolved owned daemon PID, observes death, and lets the fixture reap the replacement daemon. No detach fallback or pause-time claim was added. |
| #94/#95 | No new budget-derived deadline was added. Polling uses bounded local process/status observations; no negotiated connection is held idle across an external-state wait. |
| Remaining classes | Audited as not applicable: provider catalogs, render/UI, STT, Windows endpoint grammar, filesystem walkers, archive/release, CAS thresholds, roster grammar, runtime-root permissions, group commit, and workflow trigger surfaces are untouched. |

The prohibited OAuth, hook-test, and daemond support files are untouched.

SHIP
