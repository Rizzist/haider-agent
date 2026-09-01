# v0.0.969 warm-by-default evidence

Date: 2026-09-01  
Branch: `lane-969-warmdef`  
Verdict: **SHIP**

## Coupling audit first

Guard #77 passed before implementation with `production=189` and `test=16`.
The archived v0.0.968 result was `188/16`, so that count citation drifted by one
production unsafe block; the guard itself remains green.

The archived `worker::manager_law_tests` claim was semantically correct but its
test count drifted from 11 to 12 after the 30-second supervisor-retention pin was
added. The current suite passed 12/12 with `--test-threads=1` and again with
`--test-threads=8`. The relevant current coupling is:

- terminal-only retirement predicate: `crates/haider-daemon/src/worker.rs:2437`;
- manager defers commands that race retirement: `worker.rs:3050`;
- slot removal and retirement acknowledgement are downstream of the owned
  `JoinSet` yield: `worker.rs:3649`;
- the supervisor rechecks durable terminal state before retirement:
  `worker.rs:4063`;
- the idle clock arms only for durable quiescence: `worker.rs:4343`;
- the exact paused-time reset/expiry proof is
  `durably_quiescent_supervisor_retires_at_the_conservative_idle_ttl` at
  `worker.rs:11681`.

The prose comment at `worker.rs:176` still says “five minutes,” while the
executable constant at `worker.rs:184` and its mutation pin both say 30 seconds;
that citation is wrong/stale. This audit relies on the constant and exact timer
proof, and leaves the prior retention lane's source territory unchanged.

No worker-supervisor code changed in this lane.

## Implemented contract

The brief's “linger needs explicit opt-in” premise was partially stale at the
lane head. General auto-spawn already selected 30 seconds, but
`haider run --start` explicitly selected an unbounded persistent lifetime and
the enum's public default still selected that unbounded variant. The lane now
uses the shared finite policy for every run action and makes the enum default
the same 30-second linger (`haider-client/src/spawn.rs:69,93,106` and
`haider-cli/src/run.rs:504`).

The environment contract is unchanged: absent means 30,000 ms, zero means the
exact authenticated child is reaped after the one-shot command, a positive
integer through 3,600,000 is the override, and `haider run` reports invalid
values. Other default-constructed front doors cannot return a configuration
error and safely fall back to 30,000 ms. Positive TTL retirement remains
daemon-owned and resettable after client attachment; `haider daemon stop`
remains the operator-owned immediate graceful exit.

Boot recovery, account loading, hub/provider/worker construction, recovered-work
handoff, and optional transports already completed before Ready. The remaining
cold seam was a pre-bind SQLite memory release. Positive-TTL daemons now retain
that already-paid boot working set at `crates/haider-daemon/src/runtime.rs:1318`
and publish `warm=true`; TTL zero and direct/unbounded daemons still release it
at `runtime.rs:1635` and publish `warm=false`.

The actual launch policy is carried from daemond configuration through the
connection and hub. `status.snapshot` adds serde-defaulted
`idle_ttl_ms: Option<u64>` and `warm: bool` at
`crates/haider-rpc/src/frame.rs:4219`; `haider status --json` projects them as
`daemon.idle_ttl_ms` and `daemon.warm` at
`crates/haider-cli/src/observe.rs:258`.

## Timing evidence

A local timing loop used the repaired stdlib-only conformance support and its
loopback fake provider. Every measured turn made exactly one provider request,
produced a contiguous JSONL stream with exactly one typed terminal, and settled
durably idle. No errors or outliers were removed. The binaries were the scoped
dev-profile builds with debug info disabled by the lane environment law on
macOS 26.6.2 / Darwin 25.6.0 arm64. Their SHA-256 digests were
`2025cecbac162454ab839c36e6b9811decb8660a9735876ac55e2f6d3f669864`
(`haider`) and
`5f7366b4a1702451639d670deda620717d9d339a8f35cf8e6c9bf23bfc25bc33`
(`haiderd`).

| Cohort | N | Median wall | MAD | Min | Max |
|---|---:|---:|---:|---:|---:|
| Fresh profile; timed run includes auto-spawn | 20 | 115.903 ms | 5.632 ms | 105.416 ms | 452.466 ms |
| First timed turn immediately after Ready, fresh profile each time | 20 | 90.528 ms | 12.349 ms | 66.755 ms | 108.245 ms |
| Same PID/generation after 5 unreported warmups | 25 | 83.910 ms | 11.583 ms | 66.507 ms | 126.999 ms |

One-minute load observations were 2.710 at start, 2.493 after the cold cohort,
2.374 after the first-after-Ready cohort, and 2.183 at end; all are below the
required 10. The first turn after Ready was 25.375 ms faster at the median than
auto-spawning the daemon and only 6.618 ms above the repeat median. The repeat
median of 83.910 ms passes the owner's 70–118 ms/call target.

All 41 timing profiles reported `daemon.idle_ttl_ms=30000` and
`daemon.warm=true`. Each was shut down with its exact `haider daemon stop`
command; every response reported `stopped_cleanly` and `process_exited=true`,
and the aggregate postcondition was `alive_after_all=false`.

## Named verification

- `worker::manager_law_tests`: 12/12 at one thread and eight threads.
- `status_runtime_fields_are_additive_in_both_client_directions`: old clients
  ignore the new fields; new clients decode their absence as `null`/`false`.
- `one_shot_status_is_exactly_one_scalar_rpc`: typed client projection includes
  30,000/true without composing watches.
- `status_snapshot_counts_sessions_without_listing_summaries`: daemon status
  emits the runtime-owned policy without changing scalar accounting.
- `observe_json_schemas_are_goldened_and_secret_free`: CLI JSON golden includes
  both daemon fields.
- `repeated_run_invocations_default_to_one_warm_daemon_until_operator_stop`:
  three default-policy processes reuse one PID, status reports 30,000/true,
  operator stop is clean, and `alive_after=false`.
- The four pre-existing real auto-spawn cleanup paths now capture their exact
  PID and affirm `alive_after=false` plus endpoint absence; the directly
  parented winner is reaped before its same assertions.
- `one_shot_reaps_only_the_daemon_it_spawned_on_success_and_bootstrap_failure`
  and `sequential_ephemeral_cli_runs_advance_profile_owned_worker_generations`:
  TTL zero still removes the exact PID/socket/lock after each invocation.
- Full `cargo test --locked` package runs are green for `haider-rpc`,
  `haider-client`, `haider-daemon`, `haider-cli`, and `haider-daemond` under the
  lane environment law. The daemon package completed 916 tests with its three
  declared live/host-only tests ignored, plus all 103 session-hub integration
  tests; the other affected packages reported no failures or unexpected
  ignores.

## CI error-registry walk

- #64: scoped `target/debug/haiderd` is 177 MiB, above the 10 MiB minimum.
- #77: closing unsafe-count guard remains `production=189`, `test=16`.
- #94: no production deadline was added; the real-process stop assertion uses
  the command's existing bounded lifecycle and kernel-confirmed exit receipt.
- #95: no new wait was introduced while a negotiated connection is open.
- Test baseline: `cargo run --locked -p xtask -- test-count` reports 4,351
  tests against the 4,351 registered floor; existing mutation/acceptance tests
  were extended without adding or removing a test.
- Forbidden territory: no changes to `oauth.rs` or `oauth_tests.rs`; the
  supplied `LANE-COMMON.md`, `LANE-BRIEF-warmdef.md`, and `turnperf/` evidence
  remain unmodified and uncommitted.

SHIP
