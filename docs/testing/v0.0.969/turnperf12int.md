# turnperf12int — turnperf12 integrated onto wave-969

Date: 2026-09-01

Lane input: `a7bdce1267928a648d1e371c49443173da24489a`

Wave input: `origin/wave-969` at `2b8beeb3ab20e83387216afbd9475cc71607ba37`

## Verdict

The integration and recovery-contract proofs pass. The result is still not shippable because the accepted trace-off single-turn wall measurement misses the owner budget, both in the standalone harness and in the full T1 gate. The tool-turn wall measurement passes.

## Merge and binding resolution

`git merge origin/wave-969` was attempted first. The worktree's Git metadata is read-only, so Git could not create `ORIG_HEAD.lock`. I reproduced the three-way merge in a writable temporary clone, resolved it there, and materialized the resulting tracked tree here. HEAD therefore remains at the lane input commit, while the working tree contains the uncommitted combined result. `git diff --check`, `cargo fmt --all -- --check`, and an explicit conflict-marker scan pass.

The binding recovery resolution is present:

- Ambiguous first provider delivery remains parked behind the typed recovery door. `kill9_after_provider_admission_exposes_typed_probe_recovery` passes.
- `pending_admission_retry` and `budgeted_workflow_admission_is_recoverable` remain the wave implementations. The exact wave versions of `turn_recovery.rs`, daemon `runtime.rs`, the CLI recovery contract tests, and `core_loop_e2e_tests.rs` compare cleanly to `origin/wave-969`.
- `session recover --probe` completes the selected recovery action and reports `resulting_state=effect_unknown`; the matrix additionally validates the exact retry-pending replacement identifier.
- Budgeted workflow admission remains recoverable. `workflow_recovery_after_budget_admission_preserves_spend_and_ordinal` passes with the wave's expected physical-attempt sequence `[1, 1, 2]`.
- The rejected fail-closed names and `first_provider_delivery_is_ambiguous` are absent from the merged crate tree.
- `actor.rs` retains the wave's provider-deadline state and route handling together with the trace lane's `provider_open_started` capture. All trace points, harnesses, matrix support, CI budget wiring, and docs are retained.

The supplied `LANE-COMMON.md`, `LANE-BRIEF-turnperf12.md`, and `turnperf/` evidence were read first and were not edited or committed. OAuth files were not touched.

## Fresh SIGKILL boundary matrix

Command:

```text
TMPDIR=/tmp python3 scripts/qa-gate/turnperf_sigkill_matrix.py --bin-dir target/debug --output /tmp/turnperf12int-sigkill.json
```

Result: **47/47 PASS**, 0 failed.

- Single shape: all 11 journal ordinals plus all 3 provider gates, 14/14.
- Tool shape: all 27 journal ordinals plus both providers' 3 gates, 33/33.
- Terminal results: 39 failure terminals and 8 success terminals.
- Recovery outcomes: 6 `not_needed`, 2 `pre_accept_boundary`, 11 `probed_then_abandoned`, and 28 `terminal_without_card`.
- Provider ledger: 55 physical request rows; maximum physical count for every matrix `(case, logical ordinal)` was 1.

The 11 parked-admission cases exercised through `--probe` were:

```text
single-journal-5
single-journal-6
single-provider-1-after_post
single-provider-1-before_headers
single-provider-1-between_chunks
tool-journal-5
tool-journal-6
tool-journal-14
tool-provider-1-after_post
tool-provider-1-before_headers
tool-provider-1-between_chunks
```

**Duplicate physical provider requests on the parked-admission path: none.** The matrix snapshots the external provider ledger before probe, after probe, and after abandon, and fails on any change. The recoverable budgeted-workflow `[1, 1, 2]` exception is separately pinned by its E2E test and is not one of these unbudgeted matrix cases.

Report SHA-256: `264371e6c9be3f44d9771917d6e3ed524063d9f0fb7e4b2751f6534652a210cd`.

## Fresh trace-off harness smoke

Command:

```text
TMPDIR=/tmp python3 scripts/qa-gate/turn_wall_harness.py --bin-dir target/debug --output /tmp/turnperf12int-trace-off.json
```

The measurement was accepted; this was not `ENV_BLOCKED`. Load averages were 2.299 at start, 2.194 at midpoint, and 2.194 at end, all below the load-4 exclusion threshold. The run used five warmups and 25 retained ABBA measurements per shape, one daemon identity/generation, 90 exact provider rows (30 single, 60 tool), and emitted zero trace records with tracing disabled.

| Shape | Wall median / owner budget | CPU median | Peak combined RSS | Result |
| --- | ---: | ---: | ---: | --- |
| Single | 64.640 ms / 61.624 ms | 7.433 ms | 109344 KiB | FAIL |
| Tool | 90.291 ms / 101.962 ms | 10.490 ms | 110912 KiB | PASS |

Report SHA-256: `4cce0d454fb75a160beda8047732b8797c66b92950ee2692978eac64b5ffd643`.

The tested tree binaries were:

- `target/debug/haider`: 103211952 bytes, SHA-256 `1e1d18ed552cddea98d7e971c2e963e277e16f689297f0ea72d5b408eb2f9592`.
- `target/debug/haiderd`: 185048656 bytes, SHA-256 `21415a06f07c24d463c62b8b30ca711af6590adb2d467b3a8737394ccbd1fa6c`; the daemon binary identity floor passes.

## Full touched-crate suites and focused pins

All full suites pass for every crate directly touched by the integration resolution:

```text
cargo test -p haider-provider  PASS
cargo test -p haider-core      PASS (229 passed, 1 manual timing test ignored)
cargo test -p haider-daemon    PASS (1018 passed, 3 live/manual tests ignored)
cargo test -p haider-daemond   PASS (146 passed)
cargo test -p haider-cli       PASS (all test binaries)
```

Focused recovery, deadline, retry, trace, and telemetry pins also pass, including:

- `only_budgeted_active_workflow_admission_is_runnable_recovery_work`
- all `session_recover::tests`
- `kill9_after_provider_admission_exposes_typed_probe_recovery`
- `workflow_recovery_after_budget_admission_preserves_spend_and_ordinal`
- `automatic_rate_limit_retry_preserves_the_terminal_deadline_margin`
- `deadline_terminal_class_is_derived_from_provider_retry_state`
- `m4_deadline_expiry_in_backoff_is_bounded_provider_failure`
- `run_jsonl_bounded_rate_limit_exhaustion_is_one_provider_terminal`
- provider trace transaction-ordinal and daemon telemetry-allowlist tests

The QA Python unit suites pass: 48 tests. The added recovery receipt test requires the exact retry-pending replacement. Registry checks pass: unsafe counts are production 189/test 16, and the test inventory is exactly 4347 against baseline 4347.

## Full T1 tree-bin gate

Command:

```text
bash scripts/qa-gate/run.sh --tier t1 --bin-dir target/debug
```

Result: **4/5 PASS, 1 FAIL, 0 SKIP, 0 ENV-BLOCKED; measurement accepted**.

- `t1.daemon.kill9_midturn`: PASS. The daemon was killed while streaming, respawned generation 1 to 2, probe produced `effect_unknown`, resume remained finitely `recovery_required`, a fresh run completed, and cleanup found no orphan.
- `t1.daemon.lifecycle_triad`: PASS. Autospawn, bounded idle exit, generation-2 respawn, explicit clean stop, PID exit, and no-orphan checks all passed.
- `t1.install.paths`: PASS.
- `t1.store.previous_release_upgrade`: PASS.
- `t1.turn.wall_budget`: FAIL only on single wall median: 66.049 ms exceeds 61.624 ms. Tool wall median passes at 90.385 ms against 101.962 ms. No orphan daemon remained.

T1 report: `docs/testing/v0.0.968/qa-gate-t1-Syeds-MacBook-Air.local-20260901T105839Z.json`, SHA-256 `d5c138b44caab5c9f7ad0d0596aecc759892bfed9325664a716c84120886b8b0`.

The recovery integration is correct and the requested kill/recovery coverage is clean, but the accepted single-turn performance miss is a binding ship-gate failure.

NO_SHIP
