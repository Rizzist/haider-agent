# Lane contract — four owner-accepted decisions (v0.0.969)

Date: 2026-09-01

## Verdict

The 2026-09-01 workflow-recovery continuation is **SHIP**: the deterministic
full-suite regression is fixed, its named E2E passes 10/10, the full
`haider-daemond` suite is green, and the unchanged T1 tier passes 4/4 against
the tree binaries. The original four-item assessment below still records the
external conformance adapter's 15-minute child deadline versus 15-second outer
kill mismatch; this continuation neither changes nor relies on that external
adapter.

## Base, workflow, and guard

- Read `LANE-COMMON.md`, `LANE-BRIEF-contract.md`, and all supplied `turnperf/`
  evidence before product work. They remain uncommitted and unedited.
- The worktree is named `lane-969-contract`. Its immutable Git worktree metadata
  still points at `9ed86565bd5ac42c9e942a5ac16fc5c6600cf8bd`; the current
  `origin/wave-969` is `82b7a1419d777aa6ea6d3931f57339e5cfb5eb1a`.
  A fast-forward could not update the externally owned worktree metadata, so the
  exact `HEAD..origin/wave-969` binary overlay was applied to the working tree
  before lane edits. Protected overlay files were verified byte-for-byte against
  `origin/wave-969` at closeout.
- Guard #77 ran before product edits and at closeout. Both runs passed:
  `production=188`, `test=16`.
- Each item was handled as reproduce/pin, fix, then focused verification before
  proceeding.

## 1. Broad auto-spawn idle TTL — PASS

Red/pin: status, non-interactive TUI, and recovery-probe auto-spawns inherited a
persistent default instead of the bounded run policy. Real-process pins exposed
that breadth gap and separately pinned that an expired idle TTL cannot retire a
daemon with a durable non-terminal run.

Implemented:

- `haider-client::spawn` now owns the one shared
  `HAIDER_RUN_DAEMON_IDLE_TTL_MS` parser and policy. The default is a 30-second
  linger, zero retains the immediate test/opt-out behavior, and positive values
  are bounded at one hour.
- Every default auto-spawn front door uses that policy. `haider run` uses the
  same parser so malformed values remain typed configuration errors.
- Idle retirement requires both zero live clients and journal-proven durable
  quiescence. A store error fails closed. Durable publication wakes a parked
  retirement check, so a run that later terminalizes allows an already-expired
  arm to drain.
- Explicit `haider daemon stop` behavior is unchanged.

Evidence:

- `real_status_tui_and_probe_autospawns_share_short_idle_ttl` — PASS with a
  real sibling daemon for status, TUI, and recovery probe.
- `real_run_short_idle_ttl_terminalizes_spawned_daemon` — PASS for the run
  auto-spawn path.
- `idle_ttl_never_retires_a_daemon_with_a_nonterminal_run` — PASS; the real
  daemon survives the expired TTL, then exits after a durable cancellation.
- Unchanged `t1.daemon.lifecycle_triad` — PASS. Only its now-green
  `expected_fail_until` was removed afterward.

Territory note: the smallest required retain-owned overlap is the accept-loop
arm/wake in `crates/haider-daemon/src/runtime.rs` and a read-only durable
quiescence query in `session_hub/mod.rs`. No store commit path was changed.

### Workflow-recovery continuation — PASS

The supplied retirement diagnosis was tested and refuted for the named E2E:

- `workflow_recovery_after_budget_admission_preserves_spend_and_ordinal` is now
  at `core_loop_e2e_tests.rs:1984`; the panic helper remains at line 1077. The
  test uses in-process `ready_with_dependencies` / `spawn_with_dependencies`,
  which installs no launcher-liveness watcher and cannot request
  `GracefulAfterIdle`.
- On the second daemon, the client is attached throughout the 60-second
  observer. The daemon diagnostic remains `Ready`, accepts the connection, and
  answers every Ping/Pong. The separate supervisor idle TTL is five minutes,
  so it also cannot expire inside this failure.
- A successful `SupervisorOutcome::QuiescentRetired` sets
  `terminalize_nonterminal=false`; it cannot emit the cited “exited supervisor
  work could not be terminalized” log. That log is teardown fallout after the
  panic removes the temporary profile while detached daemon work is unwinding.
- The pre-fix replay proves startup recovery did run: it appended effect
  intent/authorization/dispatch/outcome-unknown, a recovery menu, and
  `RunState::EffectOutcomeUnknown` at sequences 17-22. It then deliberately
  enqueued no worker recovery, which is why the attached observer saw no new
  events.

Actual cause: item 2 had changed every response-free first provider admission
from `RecoveredWork::AdmissionRetry` into explicit effect reconciliation. That
broad rule collided with the existing max-cost workflow contract, whose
budgeted admission must restore logical spend/request ordinals and re-enter the
worker.

Implemented reconciliation:

- Startup retains effect-outcome-unknown parking for ordinary ambiguous
  provider admissions, including unchanged `t1.daemon.kill9_midturn`.
- Automatic admission recovery is restored only when both durable coordinates
  are present: a non-empty run budget and an active workflow graph. This is the
  exact composite shape exercised by the failing max-cost workflow E2E.
- `RunReduction` now retains the durable `RunBudgetV1`. The startup checkpoint
  reducer version is bumped from v6 to v7 so an older checkpoint cannot hide
  the newly authoritative budget discriminator.
- No retirement predicate weakening was needed. The daemon accept loop already
  requires zero attached clients, and `daemon_is_durably_quiescent` fails
  closed on store errors and rejects every durable non-terminal run. Valid
  workflow continuations are `Streaming`; admitted pre-response requests are
  `Thinking` or response-free `Streaming`, so both already block retirement.

New pins:

- `only_budgeted_active_workflow_admission_is_runnable_recovery_work` proves
  the two-dimensional recovery discriminator: unbudgeted, missing-graph, and
  completed-graph shapes remain explicit-reconciliation work; only the
  budgeted active workflow is re-enqueued.
- `recoverable_workflow_continuation_blocks_daemon_idle_ttl_retirement` seeds
  an active graph plus durable headless configuration, completed provider
  request-attempt, `Streaming`, and `GraphFinalizationDeferred` facts. It
  reopens the store, proves startup classifies exactly one runnable workflow
  continuation with the preserved spend/ordinal coordinates, and proves that
  recovered work still makes the daemon retirement predicate false. The
  truly-idle positive control remains the real-process `lifecycle_triad` pin.

Continuation evidence:

- Pre-fix isolated reproduction — FAIL in 60.14 seconds with
  `last_state=None`, `failure=None`, `observed_events=0`.
- Post-fix isolated E2E — PASS once, then PASS 10/10 in a dedicated exact,
  single-thread loop; each stress iteration completed in 0.21-0.23 seconds.
- Full `cargo test --no-fail-fast -p haider-daemond --locked --
  --test-threads=4` — PASS, 145 tests, including all 18 core-loop E2Es.
- Unchanged `bash scripts/qa-gate/run.sh --tier t1 --bin-dir target/debug` —
  PASS, 4/4. `lifecycle_triad` observed the idle exit and respawn;
  `kill9_midturn` retained `probe` / `effect_unknown`, consumed no fresh-run
  sentinel, and left no orphan daemon. Report:
  `docs/testing/v0.0.968/qa-gate-t1-Syeds-MacBook-Air.local-20260901T060421Z.json`.
- Tree `haiderd` is 184,728,112 bytes, above the 10 MiB integrity floor.

## 2. Kill-9 recovery probe — PASS

Red/pin: the real kill-9 check restarted the daemon and received
`no_recovery` / `no crash window`; the fresh provider sentinel was consumed.

Implemented:

- Startup recovery recognizes an admitted provider request with no durable
  response as an ambiguous effect. It records the sanctioned recovery effect
  lifecycle and exposes the standard recovery menu with
  `run_state=effect_outcome_unknown`; it does not automatically re-dispatch the
  provider request.
- `session <id> recover --probe` returns the typed recovery card with exit 0.
  `no_recovery`/77 is now reserved for a clean idle terminal. A non-terminal
  session that lacks a complete card returns typed `recovery_incomplete`/69 and
  remains retryable.
- `automation-contract-v1.md` documents the card, receipt, error shapes, exit
  codes, kill-9 behavior, and an additive changelog entry.

Evidence:

- `kill9_after_provider_admission_exposes_typed_probe_recovery` — PASS with a
  real daemon kill -9 and restart; schema `haider.session_recovery.v1`, completed
  probe, `effect_unknown`, exact replacement menu id, and no fresh sentinel.
- Session-recovery unit set — 4 PASS.
- Unchanged `t1.daemon.kill9_midturn` — PASS. Only its now-green
  `expected_fail_until` was removed afterward.
- The final T1 report is
  `/tmp/haider-969-contract-final-t1/qa-gate-t1-Syeds-MacBook-Air.local-20260901T050159Z.json`:
  4 PASS, 0 FAIL, 0 SKIP, 0 environment-blocked.

Protected territory: `crates/haider-core/src/actor.rs` was not lane-edited. Its
local SHA-256 equals `origin/wave-969`:
`1ce9c0843590ca940d0cfa5196284a8b87bc562b5befc0583860029c8659cbce`.
The upstream retry E2E fixture likewise matches the wave byte-for-byte.

## 3. SIGINT durable cancellation — PASS

Red/pin: the QA case was an `expected_gap` skip and the headless client had no
signal-to-durable-cancel/drain contract.

Implemented:

- The first SIGINT after run correlation sends one stable, idempotent
  `turn.cancel`, waits for its durable receipt, and continues reducing the
  stream until the durable cancellation terminal. The wait is bounded by the
  tighter caller/run deadline.
- The ordinary typed cancellation terminal is printed once and the CLI exits
  130.
- A second SIGINT, after the durable cancel receipt, takes the immediate exit
  130 path and deliberately leaves that cancel in place.
- Headless attach uses the same public interrupt channel and reducer path.
- `jsonl-run-contract-v1.md` and `automation-contract-v1.md` document the
  contract and additive changelog.

Evidence:

- `pending_second_interrupt_is_consumed_before_terminal_drain` — PASS.
- The isolated, otherwise unchanged `t0.run.exit_codes` check — PASS after the
  required single switch `SIGINT_EXPECTED_GAP = False`; it reports signal
  `SIGINT-to-client`, exit 130, and one cancellation terminal.
- A real-daemon double-SIGINT E2E exited 130 with exactly one durable cancelling
  fact, durable terminal `cancelled`, and one streamed typed cancellation
  terminal.
- QA loader tests — 35 PASS with a short `TMPDIR`.

The full T0 tier was not claimed: after the acceptance check passed early, that
run was stopped during an unrelated long TUI palette check. The named SIGINT
acceptance itself completed green.

## 4. Caller deadline vs. response-open watchdog — product PASS, required bench FAIL

The current wave overlay already contains the intended product seam, which this
lane retained and verified:

- OpenAI-family response-open remains 60 seconds by default.
- `effective_request_budget` selects the provider budget or the remaining
  caller/run deadline minus the existing one-second terminalization reserve,
  whichever is tighter.
- The daemon scopes provider opening to the tighter request/run deadline. A
  never-opening provider terminalizes with typed `provider_timeout` before the
  caller deadline.

Focused evidence:

- Default response-open configuration and 60-second telemetry pins — PASS.
- `effective_request_budget_obeys_provider_and_run_deadlines` — PASS; a
  15-second caller budget gives a 14-second response-open allowance, while no
  caller deadline retains 60 seconds.
- `never_opening_provider_terminalizes_before_headless_run_deadline` — PASS in
  approximately two seconds with the typed provider-timeout terminal.

Required unchanged external run:

```text
cd /Users/rizzist/Documents/CODING/haidercode-web
python3 -m bench.conformance --adapter haider-agent \
  --model deepseek-v4-flash --context-window 131072 \
  --max-output-tokens 8192 --max-turns 20 \
  --executable /Users/rizzist/haider-run/lane-969-contract/target/debug/haider \
  --process-timeout 15 --proxy-timeout 20 --round-index 0 \
  --json-report /private/tmp/haider-969-contract-conformance.json
```

Result: **FAIL**, 18 PASS, 1 FAIL, and 2 expected macOS platform skips. The
`timeout` case was killed with exit -9 after 15.009 seconds and one proxy
request; no Haider terminal had time to arrive.

Cause: the unchanged external
`bench/adapters/haider-agent/adapter.toml:52` passes
`[... "--timeout", "15m", ...]`. The runner's `--process-timeout 15` is an
outer harness kill budget and is not supplied to the child as a Haider caller
deadline. Therefore the effective product response-open limit is correctly
`min(60s, 15m - reserve) = 60s`, which cannot satisfy a 15-second harness kill.

Diagnostic only: a temporary adapter copy under `/private/tmp` changed only the
child `--timeout` from `15m` to `10s`. The same suite then reported
`PASS (platform skips)`: 19 PASS, 0 FAIL, 2 expected skips; the timeout case
returned a structured error with exit 65 after 9.013 seconds and one request.
Report: `/private/tmp/haider-969-contract-conformance-10s.json`. Its different
adapter-manifest hash means it is proof of the product seam, not acceptance.

The minimal remaining fix is outside this writable lane: make the bench adapter
pass a child deadline strictly below its process deadline (or derive the child
deadline from the runner setting). Weakening the 60-second product default or
adding a bench-specific inference would violate the accepted policy.

## Verification and repository integrity

All Cargo commands used the mandated 8 MiB stack and discovery/test environment;
daemon-spawning tests used prebuilt siblings.

| Check | Result |
| --- | --- |
| Guard #77, initial and final | PASS, production 188 / test 16 |
| Final qa-gate T1 | PASS, 4/4 |
| Isolated qa-gate `t0.run.exit_codes` | PASS |
| Idle-TTL real-process pins | PASS |
| Kill-9 real-daemon E2E | PASS |
| SIGINT unit and real-daemon E2E | PASS |
| Provider default/deadline pins | PASS |
| Automation contract golden test | PASS |
| `cargo clippy -p haider-client -p haider-cli -p haider-daemon --all-targets -- -D warnings` | PASS |
| `cargo fmt --all -- --check` | PASS |
| `cargo metadata --locked --no-deps` | PASS |
| `git diff --check`, unmerged-index and conflict-marker scans | PASS |
| `cargo run -p xtask -- check` | PASS, 4,336 / 4,336 tests |
| Built `haider` | 103,083,472 bytes |
| Built `haiderd` | 184,728,112 bytes; exceeds 10 MiB |
| Unchanged external conformance suite | FAIL, timeout case killed by outer 15-second budget |

No manifest or lockfile changed. No TUI crate, store crate, OAuth file, or
store-commit path differs from `origin/wave-969` because of this lane.

## Baseline recount

The uncommitted four-item lane stood at a 4,334-test baseline before this
continuation. The continuation adds exactly two Rust test attributes: the
budgeted-workflow recovery discriminator and the production-shaped recoverable
workflow retirement pin. `test-baseline.txt` is therefore 4,336;
`cargo run -p xtask -- check` confirms 4,336 / 4,336 against that exact
baseline.

## Citation audit

The contract brief contains named constructs but no numeric `file:line`
citations. Every named construct was located by grep before use. Current source
locations include the shared auto-spawn policy in `haider-client/src/spawn.rs`,
durable retirement in `haider-daemon/src/runtime.rs` and `session_hub/mod.rs`,
probe classification in `haider-cli/src/session_recover.rs`, kill-9 parking in
`haider-daemon/src/turn_recovery.rs`, and headless interrupt handling in
`haider-client/src/headless.rs`.

The common brief's base citation is drifted/wrong for this checkout: it names
`wave-969 @ 8952219`, while current `origin/wave-969` is `82b7a141`. Its heading
also still says “968 lane.” Neither stale statement was used as source truth.

## CI error registry walk

| Registry class | Result |
| --- | --- |
| #1-#6 | Checked. The additive public `HeadlessInterrupt` API compiles on all affected targets; existing headless APIs wrap it, and visibility/import ownership is explicit. |
| #7-#19 | Checked. No manifest/lock change, mutation residue, lint allowance, dead code, formatting error, or unresolved diagnostic; affected-target deny-warnings Clippy is green. |
| #20/#21 | Fixed/checked. Baseline is 4,336 and every Cargo test used the required 8 MiB stack. |
| #22-#28 | Checked: no tracing-global, migration/schema, provider authority, filesystem/platform, Windows wire, or generic process-runner contract changed. Windows signal plumbing is by source inspection. |
| #29 | Fixed: every real auto-spawn front door now has a bounded shared lifetime and a black-box pin. |
| #30-#44 | Checked. Added waits are finite and report durable state; no Android/release/catalog/collection/socket class changed. |
| #45/#77 | Passed before and after edits at production 188 / test 16; no unsafe block was added. |
| #46-#63 | Checked. The only runtime/session-hub overlap is the named, journal-authoritative quiescence check; no store CAS/commit or TUI behavior changed. |
| #64/#67/#71/#72/#74 | Prebuilt sibling tests used the required environment; `haiderd` is 184,728,112 bytes; discovery stayed disabled and test roots were isolated. |
| #65/#68-#78 | Fixed/checked. Probe, provider-timeout, cancellation terminal, and exit-code assertions are typed; no error is swallowed or reclassified. |
| #79-#93 | Fixed/checked. Signal/process ownership, immediate second-SIGINT behavior, PID identity, durable publication wake, and replay evidence have named tests. No sparse-file, line-ending, or sampling change. |
| #94 | Checked. New test waits state their arithmetic; product request opening uses the existing one-second terminalization reserve and the tighter absolute deadline. |
| #95 | Checked. First-SIGINT drain continues consuming the negotiated stream, so the existing client protocol continues servicing connection traffic. |
| #96-#98 | Checked. The one-second terminal-delivery reserve, route attribution, and durable recovery/replay batching are retained. |

No new CI error class was discovered. The external adapter deadline mismatch
remains an external harness issue documented by the original assessment; the
requested workflow-recovery continuation is green.

SHIP
