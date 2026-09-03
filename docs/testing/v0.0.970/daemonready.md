# daemonready — positive daemon readiness (lane-970-daemonready)

Verdict: **SHIP**.

Tested on macOS on 2026-09-03 from `lane-970-daemonready` at `6a374b1` plus
the uncommitted lane diff. The owner-supplied `LANE-COMMON.md`,
`LANE-BRIEF-daemonready.md`, `turnperf/`, and `turnperf2/` evidence remain
untracked and unchanged.

## Investigation and decision

The AHRB race is real, with one naming qualification. The early process marker
is the profile-lock owner record (`lock.owner`), written by
`ProfileLock::acquire` before store open and startup recovery. The daemon-owned
`haiderd.pid` file is written later by `endpoint::bind`, after recovery
and registry setup, but neither file is a serving-readiness contract. The
launcher readiness pipe was already event-driven, so the correction preserves
that mechanism and changes the event's authority to one shared positive
predicate.

`Readiness::snapshot` is now the sole predicate. It is true only when all of
these facts hold at once:

1. the store opened;
2. startup recovery completed;
3. provider descriptors and factories were loaded;
4. the session hub reached its turn-acceptance boundary; and
5. the live lifecycle phase is `Ready`.

`ready_since` is set to Unix epoch milliseconds at the first publication of
that positive edge and is absent while the predicate is false.
`providers_loaded` exposes only registry/factory readiness. It does **not**
claim authentication, reachability, or a connected upstream provider;
providers connect per request.

The same snapshot gates the launcher readiness pipe, the handshake readiness
phase, `status.snapshot`, `haider status --json`, and `haider --ready`.
`haider --ready` now obtains the typed status snapshot and succeeds only for a
positive predicate with its timestamp and registry fact. PID files, sockets,
or a legacy ready-looking handshake cannot synthesize success.

The injected delay used by process tests sits after endpoint/PID publication
but before the session-hub-accepting fact and final `Ready` publication. The
daemon entry point accepts `HAIDER_TEST_READY_DELAY_MS` only when the fake
provider test environment is also enabled, and caps it at ten seconds.

No idle-TTL, warm-retention, provider connection, store recovery, or recovered
work ordering was changed. The startup awaits found in the supplied turnperf
and turnperf2 evidence remain before the positive edge. There is no polling
loop or fixed 25 ms launcher tax.

Territory overlap: the retain lane names the session hub. This lane makes the
smallest necessary edits in `session_hub/mod.rs` and `session_hub/rpc.rs` to
thread the shared readiness snapshot into `status.snapshot`; it does not alter
worker-supervisor retirement, idle retirement, cache admission, or observation
retention. The orchestrator must reconcile those two narrow seams.

## Contract and compatibility audit

- `ResponseBody::StatusSnapshot` adds default-compatible `ready_since` and
  `providers_loaded` fields. Older JSON without them still decodes as
  `None`/`false`; old-runtime handling retains its existing welcome fallback
  for the legacy `ready` value only.
- `haider status --json` projects the additions as
  `daemon.ready_since` and `daemon.providers_loaded`; the existing
  `daemon.ready` field now carries the positive serving predicate.
- The RPC fixture, wire compatibility golden, client projection, CLI JSON
  fixture, status smoke, one-shot boot, and autospawn assertions were updated
  together.
- `docs/client-contract-v1.md` and `docs/automation-contract-v1.md` define the
  additive fields and contain a dated v0.0.970 changelog entry.
- The CI error registry delta walk was appended without weakening an existing
  class. No OAuth source/test, dependency, manifest, feature token, or package
  version was changed.
- Unix process behavior was executed on macOS. The shared readiness latch and
  serde/RPC semantics are platform-neutral by inspection; Windows and Linux
  process behavior is therefore labelled **by inspection**.

## Required behavior tests

| Requirement | Evidence |
| --- | --- |
| No premature `Ready` publication | `lifecycle::tests::ready_publication_requires_every_startup_prerequisite` uses a fresh publisher for each of the four facts, proves publication is refused when any one is omitted, then proves the all-present case succeeds. |
| False during slowed initialization, true after | `readiness_is_false_during_injected_pre_ready_pause_and_true_after` waits for the real daemon PID file during the injected pause, observes `Recovering`, `ready=false`, no timestamp, and the already-loaded registry; after startup it proves the shared snapshot and real `status.snapshot` carry `ready=true`, a timestamp, and `providers_loaded=true`. |
| Immediate-spawn client race | `clients_immediately_after_daemon_pid_publication_all_wait_and_succeed` waits for the earlier profile-lock owner PID, starts four concurrent `haider run` clients while the starter is still delayed, and proves all four finish successfully against one daemon process. |
| `haider --ready` and launcher pipe alignment | `launcher_readiness_pipe_stays_silent_until_positive_predicate` observes the inherited pipe directly after daemon PID-file publication, proves it stays silent inside the injected pause, and then receives the positive-edge byte. The four-client race separately proves `haider --ready` waits and succeeds. |
| Additive wire compatibility | `status_runtime_fields_are_additive_in_both_client_directions` decodes an old response with defaults and proves a legacy decoder ignores the new fields. Client and CLI observe tests pin both typed and JSON projections. |
| Existing status/boot behavior | `status_discovery_smoke_tests`, `oneshot_boot_tests`, and the complete affected-crate suite passed with the new fields asserted. |

## Verification record

All Cargo verification used `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, and `CARGO_PROFILE_DEV_DEBUG=0`. Daemon-spawning tests
also used `HAIDER_TEST_SIBLINGS_PREBUILT=1`. Free space was checked before
builds; the final check showed 14,833,536 KiB available. Freshly built binaries
measured 103,789,552 bytes for `haider` and 185,839,616 bytes for `haiderd`, so
the daemon exceeds the 10 MiB guard.

- Full affected suite:
  `cargo test --quiet -p haider-platform -p haider-rpc -p haider-client
  -p haider-daemon -p haider-daemond -p haider-cli`: **passed**, including
  926 daemon library tests with the three pre-existing live-provider ignores
  and every process integration group.
- Fresh-binary slowed-init test: **1 passed**.
- Fresh-binary four-client immediate-spawn race: **1 passed**.
- Direct launcher readiness-pipe silence/positive-edge test: **1 passed**.
- `cargo test --quiet -p haider-platform`: **33 unit tests plus 2 ancillary
  groups passed**.
- Scoped all-target Clippy for `haider-platform`, `haider-daemon`,
  `haider-rpc`, `haider-client`, `haider-cli`, and `haider-daemond` with
  `-D warnings`: **passed**.
- `bash run.sh test` from `scripts/qa-gate`: **64 passed**. This checkout has
  no root `run.sh`; `scripts/qa-gate/run.sh` is the maintained gate.
- `cargo run -p xtask -- test-count`: **4,441**, matching the updated 4,441
  baseline.
- `git diff --check`: **passed**.
- Repository-wide `cargo fmt --all -- --check` reports only pre-existing,
  unrelated formatting in `haider-protocol/tests/schema_changelog_tests.rs`,
  `haider-tui/tests/tpsfix_widget_tests.rs`, and
  `haider-tui/tests/ui_polish_tests.rs`; no lane-changed Rust file is in that
  report.
- The standalone unsafe-count script reports four test-only `haider-tui`
  occurrences against a zero baseline. `git diff --quiet -- crates/haider-tui`
  passes, confirming this lane neither introduced nor changed them; changing
  another lane's code or baseline would violate the scoped brief.

## Citation and evidence audit

The daemonready brief contains no inherited literal `file:line` citations, so
there is no numbered citation to carry forward. Constructs were grep-located
against the actual `6a374b1` base.

| Finding | Classification | Audit result |
| --- | --- | --- |
| A PID is published before initialization completes | **Correct with qualification** | `ProfileLock::acquire` writes `lock.owner` before store open/recovery. This is the boundary exercised by the four-client race. |
| The daemon-owned `haiderd.pid` is that early marker | **Wrong if interpreted this way** | `endpoint::bind` publishes it later, after store/recovery/registry work. The slowed-init hook intentionally delays the final accepting edge after this publication. |
| Launcher notification is event-driven | **Correct** | `spawn_daemon_process` waits on the inherited readiness stream. The runtime now writes that signal only after `Readiness::snapshot().ready`. |
| Existing status output proves positive readiness | **Drifted before this lane; corrected here** | The prior daemon status path hard-coded `ready=true`. `SessionHub::handle_status_snapshot` now reads the shared snapshot. |
| Provider-loaded means provider connected | **Wrong** | Only descriptors/factories exist at the registry boundary; network/auth connections remain per request. The contract explicitly says so. |

## CI registry walk

The appended v0.0.970 delta walk in
`scripts/qa-gate/CI_REGISTRY_WALK_QAGATE3.md` covers registry classes #1-#96.
The readiness state is typed and additive; the test hook is bounded and
test-provider-gated; no deadline, keepalive, OAuth, ownership, discovery,
provider terminal, TTL, or warm-lifetime behavior changed. The four-client
test owns and reaps every spawned process and uses the existing derived process
budgets.

## Independent verification

Three read-only audits reviewed the completed lane. The research audit
confirmed the startup/PID citation nuance, shared predicate, provider-registry
meaning, and unchanged TTL/warm path. The test auditor initially withheld SHIP
because the first race test could not independently detect an early pipe byte
and the prerequisite mutation check omitted only one fact; after the direct
pipe test and four-way omission matrix were added, its re-audit returned SHIP.
The closing verifier inspected the final platform seam, affected-crate tests,
Clippy, contracts, and cross-lane note and also returned SHIP.

SHIP
