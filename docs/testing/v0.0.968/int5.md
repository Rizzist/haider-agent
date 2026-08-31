# int5: qagate4 checks ported onto the current QA gate

## Outcome

The source review used merge base `f0a4c289763345a459a3ad8d05ea322254885051`
and commit `2f745b1`; the old branch was not merged. Four of its six T1 checks were
ported to the current runner. Two wrapper/global checks were deliberately dropped
because the current gate has stronger, directly owned equivalents.

| qagate4 check | Decision | Current-gate reason |
| --- | --- | --- |
| `t1.daemon.kill9_midturn` | ported | Pins the live kill-9 recovery defect while preserving the current runner's exact PID ownership and strict semver metadata. |
| `t1.daemon.lifecycle_triad` | ported | Expresses the autospawn, idle retirement, generation increment, respawn, and typed stop contract. |
| `t1.install.paths` | ported | Exercises the real installer into scratch and then proves the installed sibling pair can start, report, stop, and disappear. |
| `t1.store.previous_release_upgrade` | ported | Uses the SHA-pinned 0.0.966 release, exact legacy-daemon ownership, migration, fresh-schema equality, and persistent fixture publication. |
| `t1.tui.ladder` | dropped | The current gate already has five direct hermetic PTY checks, while the ship gate owns the legacy ladder. Folding that wrapper into T1 would duplicate weaker observations. |
| `t1.daemon.no_orphans_after_suite` | dropped | Every current check has mandatory cleanup based only on its status-owned PIDs. The old suite-global census could attribute unrelated installed daemons to this run and never had signal authority. |

No product source, CI workflow, ship-gate job, generated report, old ladder script,
or old runner replacement was carried from qagate4.

## Minimal runner additions

- Added the product's named 5-second daemon drain budget.
- Generalized the existing command executor to support a chosen binary, working
  directory, and hermetic environment overrides while retaining process-group
  timeout/reap behavior.
- Added fake-script replacement for finite post-crash segments.
- Allowed status observation under an explicitly supplied scratch root and allowed
  sequential daemon PIDs only after the prior PID has retired. Concurrent live PIDs
  still fail ownership.
- Added a legacy status observer that trusts only the exact short-root PID file plus
  exact executable identity. A mismatch is never signalled or stopped.
- Added report-relative artefact publication and a `network:github` need that becomes
  `ENV_BLOCKED` under `HAIDER_QA_GATE_OFFLINE=1`.
- Preserved the current runner's strict semantic-version validation, per-check
  cleanup, interrupt helper, isolated subcases, TUI APIs, and expanded spawn-family
  recognition.

Each capability has a self-test in `scripts/qa-gate/tests/test_runner.py`.

## Budget ledger (registry #94)

Every new check exports a `BudgetSum`; no new check contains `time.sleep`.

| Check | Derived milliseconds | Derivation |
| --- | ---: | --- |
| `t1.daemon.kill9_midturn` | 751,000 | startup/status, continuous 30s active-state arm, kill observation, respawn, five-request recovery path, finite resume/fresh turn, typed stop, and per-observed-PID cleanup. |
| `t1.daemon.lifecycle_triad` | 298,000 | first startup/status + 1s TTL + 5s drain + 2s PID observation + respawn/status + stop + two historical-PID cleanup observations. |
| `t1.install.paths` | 408,000 | two outer 120s GitHub fetch allowances + installer work + installed version/status/stop/PID observation + cleanup. The allowance is the gate's outer bound, not an installer-internal timeout. |
| `t1.store.previous_release_upgrade` | 901,000 | pinned download, two old turns, legacy status/stop/drain, upgraded list/turn/status/stop, fresh status/stop, SQLite comparisons, fixture publication, and three-PID cleanup. |

New T1 total: `2,358,000ms`. Existing fourteen-check T0 total:
`18,192,000ms`. The self-test pins the complete eighteen-check total at
`20,550,000ms`.

## Product contracts and citation-drift audit

| qagate4 claim/citation | Verdict on this tree | Evidence used by the port |
| --- | --- | --- |
| Ordinary request deadline is 60s (`client.rs:43-46`) | correct | `crates/haider-client/src/client.rs:43-46` defines the continuous correlated-request deadline. |
| Daemon startup is 30s (`spawn.rs:58`) | correct | `crates/haider-client/src/spawn.rs:57-58`. |
| Recovery is a bounded multi-request path | correct construct, line range still usable | `crates/haider-cli/src/session_recover.rs:266-282,455-520` shows digest, attach, and request calls used by the recovery command. |
| Headless resume accepts an explicit finite timeout (`automation.rs:204-240`) | correct | `crates/haider-cli/src/automation.rs:204-240` parses and stores `--timeout`, `--no-spawn`, and required `--json`. |
| `HAIDER_RUN_DAEMON_IDLE_TTL_MS` is parsed at `run.rs:488-513` | correct parser, incomplete lifecycle claim | `crates/haider-cli/src/run.rs:488-512` parses the TTL. The T1 contract deliberately requires an autospawned daemon to honor idle retirement; installed 0.0.967 does not. |
| Daemon drain is 5s | correct | `crates/haider-daemon/src/config.rs:107-109`. |
| A new daemon advances generation | correct | `crates/haider-daemon/src/runtime.rs:807-814`; idle shutdown is armed at `631-656` and evaluated at `1825-1846`. |
| Previous schema version is 26 | wrong | The authoritative 0.0.966 release profile and current `crates/haider-store/src/migrations.rs:22-23` both show schema 27. Migration is idempotent at `1160-1192` and incremental/transactional at `1239-1257`. The check asserts observed 27, not the stale brief value. |
| Installer fetches have their own timeout | wrong | `scripts/install.sh:18-25` calls curl/wget without a timeout. The two actual asset fetches are `74-75`; the gate therefore names its 120s values as outer process allowances. Version, target, checksum, and sibling install logic are at `50-119`. |

Cross-lane reconciliation note: retainfix commit `e8f9e04` documents the narrower
`haider run` short-TTL path as already passing and makes no product change. This port
preserves qagate4's broader status-autospawn retirement contract because the int5 brief
explicitly requires that check and its 0.0.968 adjudication. The orchestrator should
decide whether 0.0.968 intends the broader all-autospawn contract or only the
`HAIDER_RUN_DAEMON_IDLE_TTL_MS` run path before merging the lanes; int5 does not
silently redefine the assigned contract.

## Installed 0.0.967 T1 evidence

Command:

```text
bash scripts/qa-gate/run.sh --tier t1 --bin-dir /usr/local/bin --report-dir /var/folders/y2/zrvhkfz54lj3gsw2czwxdmsh0000gn/T/haider-int5-t1-final.XXXXXX.9TgwbILw0Z
```

Every runner row, including its evidence line:

```text
FAIL t1.daemon.kill9_midturn session recover --probe expected_exit=0 actual=77 timed_out=false actual_code='no_recovery' actual_message='no crash window to reconcile — run_state is errored'; kill9 recovery defect expected=probe/effect_unknown actual=no_recovery/errored; no_orphan_daemons pids=49203,49210 stop=not_running alive_after=false
FAIL t1.daemon.lifecycle_triad idle-exit defect expected=pid_gone within=8000ms (1000ms TTL + 5000ms drain + 2000ms grace) actual_alive=true pid=49218; respawn pid expected!=first(49218) actual=49218; daemon.generation expected=2 actual=1; no_orphan_daemons pids=49218 stop=not_running alive_after=false
PASS t1.install.paths installer_exit=0 scratch_home=true prefix=/private/tmp/haider-probe-qa-5i35tme6/install-prefix/bin pair_present=true version=0.0.967 ready_exit=0 status_ready=true daemon_pid=49432 stop=stopped_cleanly pid_gone=true; no_orphan_daemons pids=49432 stop=not_running alive_after=false
PASS t1.store.previous_release_upgrade source=v0.0.966 sha256=69735f821cb4406f12baad5d2e10981182a260edf1e52d35081d1e964b30dd6e old_user_version=27 old_sessions=2 listed_after_upgrade=true current_turn=done new_user_version=27 fresh_user_version=27 sqlite_master_equal=true schema_sha256=30c2e1b731ad616d7368972d381de3fb8529a3bc1bfd2fe9631d8cc4adac4d5a fixture=next-release-profile-v0.0.967-from-v0.0.966.tar.xz; no_orphan_daemons pids=49977,50123,50128 stop=not_running alive_after=false
report /var/folders/y2/zrvhkfz54lj3gsw2czwxdmsh0000gn/T/haider-int5-t1-final.XXXXXX.9TgwbILw0Z/qa-gate-t1-Syeds-MacBook-Air.local-20260831T214343Z.json
qa-gate t1 0.0.967: 2/4 PASS, 2 FAIL, 0 SKIP, 0 ENV-BLOCKED, measurement accepted
```

Both FAIL rows are real contract failures and retain
`expected_fail_until="0.0.968"`; the metadata does not rewrite their status.
`run.sh validate` returned `VALID ... schema=haider.qa-gate.v1 checks=4`.

## Full installed 0.0.967 T0 loop

Final report:
`/var/folders/y2/zrvhkfz54lj3gsw2czwxdmsh0000gn/T/haider-int5-t0-rerun.XXXXXX.e3OKJ8j1Px/qa-gate-t0-Syeds-MacBook-Air.local-20260831T211030Z.json`.
Validation returned `VALID ... schema=haider.qa-gate.v1 checks=14`.

All fourteen rows and the exact values used for classification:

| Row | Status | Evidence values |
| --- | --- | --- |
| `t0.account.alias_selects` | PASS | `selected=qa-b`, `a_requests=0`, `b_requests=1`, `persisted_account_alias=qa-b`, `alive_after=false`. |
| `t0.budget.max_cost_binds_before_request` | FAIL, expected through 0.0.968 | control `requests_made=1 expected=1`; below-bound `requests_made=1 expected=0 defect=budget_bound_after_exchange`; both isolated cleanups report `alive_after=false`. |
| `t0.budget.max_tokens_binds` | FAIL, expected through 0.0.968 | control `requests_made=1 expected=1`; below-bound `requests_made=1 expected=0 defect=budget_bound_after_exchange`; both isolated cleanups report `alive_after=false`. The 0.0.968 source preflight contract exists in `crates/haider-daemon/src/run_budget_tests.rs:1153-1175`; installed 0.0.967 is the adjudicated baseline. |
| `t0.daemon.status_stop` | PASS | `ready=true`, `version=0.0.967`, `stop=stopped_cleanly`, `pid_gone=true`, `second_exit=69`, `spawned=false`. |
| `t0.headless.input_required_is_typed` | PASS | `exit=0`, `input_resolution=no_human_available`, `typed_terminals=1`, `run_state=done`, `continuation=true`. |
| `t0.run.exit_codes` | SKIP | provider error `65`, timeout `124`, max-time budget `77`, missing credential `65`; the documented signal gap observed `exit=-2`, no cancellation terminal. |
| `t0.run.jsonl_contract` | PASS | `head_seq=1`, `envelopes=18`, `contiguous=true`, `terminal_kind=success`, `finite_segments=1`. |
| `t0.run.replay_resume_recover` | PASS | ordered replay, one replay document, `replay_provider_requests=0`, `journal_unchanged=true`, `control_requests=1`, `recover_code=no_recovery`. |
| `t0.sessions.wait_ready_n` | PASS | `document_count=1`, `ready_count=3`, `state_counts=idle:2,running:1`. |
| `t0.tui.catalog_help_command_list_pin` | FAIL, expected through 0.0.968 | `COMMANDS count=41`, `order_equal=true`, `missing_from_help=['attach']`, `absent_from_COMMANDS=['monitors']`, clean PTY exit. |
| `t0.tui.login_paths` | PASS | API/method/refusal/custom paths all reached at `118x36,80x24`; secret masked; journal session/event deltas zero; clean PTY exit. |
| `t0.tui.model_picker_cardinality` | PASS | `top_rows=36` at both sizes, `unique_api_slugs=29`, `oauth_pairs=2`, `placeholders=5`, all 36 targets activated, no unreachable target, `alive_after=false`. |
| `t0.tui.palette_activation_closure` | FAIL, expected through 0.0.968 | `/login` has `actual=false`; provider remains at stage 0 but `stage1_key_card=false`; all PTYs exit clean and cleanup reports `alive_after=false`. |
| `t0.tui.three_door_parity` | PASS | model/effort/fast/rename/compact agree across palette, typed, and RPC doors; `client_owned_count=35`, `wrong_outcomes=[]`, `daemon_mutation_rows=false`. |

Summary: `9/14 PASS, 4 FAIL, 1 SKIP, 0 ENV-BLOCKED, measurement accepted`.
A direct JSON query returned `unadjudicated_failures=0`; all four FAIL rows have
strict `expected_fail_until=0.0.968` metadata.

## Mutation proof and guards

The JSONL check was temporarily changed to expect terminal kind
`qa_gate_deliberately_broken`. The isolated run failed with the actual value and
still cleaned its daemon:

```text
FAIL t0.run.jsonl_contract terminal_kind expected=qa_gate_deliberately_broken actual=success; no_orphan_daemons pids=26958 stop=stopped_cleanly alive_after=false
```

The expectation was restored before the final self-test and installed runs.

- `bash scripts/qa-gate/run.sh test`: 35/35 PASS.
- `git diff --check`: PASS.
- Installed inputs: arm64 Mach-O; `haider` 34,452,224 bytes and `haiderd`
  51,052,432 bytes (the daemon exceeds the 10 MiB truncation sentinel).
- No Cargo build was run, as required for this installed-binary gate lane.
- No fixed sleeps were added to T1; state waits use named deadlines.
- Every new row is followed by the current runner's status-owned no-orphan evidence.

## CI error-registry walk

The complete 1-95 registry was walked. This Python/Markdown-only lane changes no
Rust API, ownership, async, schema, platform binding, dependency, Cargo, release,
workflow, TUI layout, or unsafe surface. Classes `1-4`, `6-9`, `11-18`, `20-22`,
`24-25`, `27-28`, `31-32`, `34-40`, `43`, `45`, `47-62`, `65-70`, `73`, `75-76`,
`78-87`, `89-90`, and `92-93` are therefore `checked: none`.

Applicable entries:

| Class | Result | Evidence |
| ---: | --- | --- |
| 5 | checked | POSIX-only kill capability is resolved as evidence; no platform-only module is imported before need resolution. |
| 10 | fixed | All new Python helpers are exercised by the 35-test runner suite or an installed T1 row. |
| 19 | fixed | Python self-tests and `git diff --check` pass; no Rust formatting surface changed. |
| 23 | checked | Migration code is read-only to this lane; the pinned release and fresh/current schemas both report version 27 and exact `sqlite_master` equality. |
| 26 | fixed | Current canonical path comparison is retained for alternate roots and exact legacy executable ownership. |
| 29 | fixed | Only status/PID-file identities inside the check's short roots grant stop/signal authority; sequential history is permitted only after retirement. |
| 30 | fixed | Both expected defect rows name expected and actual recovery/lifecycle values. |
| 33 | fixed | Runner capabilities are additive and self-tested; the current strict-semver and report contracts win. |
| 41 | fixed | Every status root is a short, throwaway, canonical path. |
| 42 | checked | Existing pair warmup remains runner-owned; measurement was accepted independently. |
| 44 | fixed | Real UDS daemon operations ran against the installed pair. |
| 46 | checked | Harness roots remain owner-controlled under the system temporary directory. |
| 63 | fixed | External release input is HTTPS plus pinned SHA-256; the installer path separately enforces its release checksum. |
| 64 | checked | Installed `haiderd` is a 51,052,432-byte arm64 Mach-O, above 10 MiB. |
| 71 | fixed | The actual installed 0.0.967 pair, not a build fallback, produced both final reports. |
| 72 | checked | Fake-provider checks explicitly disable discovery/update behavior in their hermetic environment. |
| 74 | fixed | Every check has scratch HOME, USERPROFILE, XDG, profile, runtime, and workspace roots. |
| 77 | fixed | Self-tests, report validation, mutation proof, and diff checks ran before verdict. |
| 88 | fixed | Successful upgrade fixtures publish only beneath the report-derived artefact directory before scratch disposal. |
| 91 | fixed | All four new rows provide non-empty, value-bearing evidence. |
| 94 | fixed | All nested deadlines are named `BudgetPart`/`BudgetSum` arithmetic and the exact totals are pinned. |
| 95 | checked | The Python gate invokes finite CLI one-shots and holds no negotiated connection while waiting on external state. |

No new CI error class was discovered.
