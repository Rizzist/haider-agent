# Haider QA gate

This is a Python 3.11, standard-library-only functional gate for an **installed**
`haider` and sibling `haiderd`. It never runs Cargo, never reads a real Haider
profile, and the T0 checks use only the daemon-owned fake provider. The entry
point pins a short temporary root before Python starts so macOS Unix socket paths
cannot silently fall back outside the check's runtime root.

## Commands

```sh
scripts/qa-gate/run.sh --tier t0 --bin-dir /usr/local/bin
scripts/qa-gate/run.sh --tier t1 --bin-dir /usr/local/bin
scripts/qa-gate/run.sh test
scripts/qa-gate/run.sh validate docs/testing/v0.0.967/qa-gate-t0-HOST-UTC.json
scripts/qa-gate/run.sh diff previous.json current.json
```

`--bin-dir` must contain executable `haider` and `haiderd` siblings. The runner
uses their absolute installed paths; it does not copy them or search `PATH`.
Reports default to `docs/testing/v<haider-version>/` and are named
`qa-gate-<tier>-<host>-<utc>.json`. `--report-dir` is available for local
mutation experiments.

Normal output has exactly one single-line verdict per check, followed by the
report path and a final summary:

```text
PASS|FAIL|SKIP|ENV_BLOCKED <id> <evidence_line>[; <evidence_line>...]
report <path>
qa-gate <tier> <version>: N/M PASS, F FAIL, S SKIP, E ENV-BLOCKED, measurement accepted|rejected(...)
```

Failing check lines also name their retained artefacts. Exit is zero exactly
when the report contains no `FAIL`; a missing declared need is `ENV_BLOCKED`,
never a product failure. Declaration/report errors exit 2.

## Check-module contract

Each `checks/<tier>/*.py` module exports:

| Export | Contract |
| --- | --- |
| `id`, `tier`, `area` | Non-empty strings; `id` begins with `<tier>.` and is globally unique in the tier. |
| `needs` | Tuple/list drawn from `binary`, `daemon`, `pty`, `network:none`, `network:github`, or `fixture:<relative-path>`. A known unavailable need produces `ENV_BLOCKED` without calling `run`; `HAIDER_QA_GATE_OFFLINE=1` explicitly blocks `network:github`. |
| `script` | List of fake-provider step objects. It is compact-JSON encoded into `HAIDER_TEST_FAKE_PROVIDER` before any process starts. |
| `turns_expected` | Explicit non-negative integer required by the segment law. This field supplements the base check contract because the runner cannot enforce that law without it. |
| `budget` | A `BudgetSum` of at least two positive named `BudgetPart` values, each with a source. An `int`, `float`, lone part, or other literal-only deadline is rejected while loading all checks, before any spawn. |
| `timed` | Boolean. Correctness always stands; an overloaded host rejects only the published timing. |
| `expected_fail_until` | Optional semantic version documenting a known-open product defect. It is report metadata only: a real `FAIL` remains `FAIL` and can flip to `PASS` without editing the check. |
| `run(ctx)` | Returns a non-empty `list[Evidence]`. Exceptions become a diagnostic runner `FAIL`, followed by mandatory daemon cleanup. |

`Evidence(label, status, evidence_line, artefacts)` accepts only `PASS`, `FAIL`,
`SKIP`, or `ENV_BLOCKED`. The label and evidence line must be non-empty, and the
evidence line must contain no CR/LF. This keeps every human verdict truthful and
single-line while the JSON preserves all individual evidence rows.

Fake-provider segments end at exactly these shipped step names:
`finish`, `error`, `error_presented`, `hang`, `premature_eof`,
`error_with_retryability`, and `malformed_frame`. All checks are loaded first;
the runner refuses `turns_expected > segment_count` before creating a context.
One daemon and one consumptive script normally belong to one check—never to a
tier or a neighbouring check. The narrow budget-control exception uses
`ctx.run_isolated_haider`: one sequential child context gets its own short
profile and copy of the same one-segment script, returns mandatory no-orphan
evidence, and is fully disposed before the primary below-bound context starts.
Isolated subcases cannot overlap or nest.

Every subprocess timeout receives a named budget or sum. The first JSONL check
adds `--timeout 30s` to the requested command because an attached run otherwise
has no finite product wall deadline. Its outer process bound is the arithmetic
`30s daemon startup + 30s run timeout + 2s terminal grace`; its declaration
also includes the cleanup request/stop/exit budgets.

## Hermetic and process laws

Each runnable check gets a fresh short `haider-probe-qa-*` root and calls the
existing `scripts/tui-probes/probelib.py` throwaway-profile refusal before any
child starts. `run.sh` pins `TMPDIR=/tmp` on POSIX before `tempfile` is imported,
which canonicalises to `/private/tmp` on macOS and stays within the Unix
`sun_path` budget.

The child environment removes ambient `HAIDER_*`, provider key/token/secret,
and colour variables, then pins:

- `HAIDER_PROFILE_DIR=<root>/p`
- `HAIDER_RUNTIME_DIR=<root>/r`
- `HAIDER_DISCOVERY_DISABLED=1`
- `HAIDER_NO_UPDATE_CHECK=1`
- `HAIDER_TEST_DEVICE_NAME=test-mac`
- `TERM=xterm-256color`
- scratch `HOME`, `USERPROFILE`, XDG directories, workspace, and `TMPDIR`

Path comparison is `realpath(abspath(...))` plus `commonpath`, never string
prefixing. Every observed top-level status `runtime_dir` must be beneath the
check's supplied runtime root; PID-file paths and POSIX socket paths must be
beneath that resolved runtime. On Windows, `socket_path` must instead be the
profile-digested `\\.\pipe\haider-*` address and is never treated as a
filesystem descendant. This deliberately fails if a POSIX product falls back
to another temporary directory.

After every runnable check, even a failed one, the runner calls
`status --json --no-spawn`. A reported PID becomes owned only after the status
schema, canonical `profile_path`, runtime-root containment, and socket/PID paths
all prove that it belongs to this throwaway context. Only then may the runner
call `daemon stop --json` or signal that PID. A foreign/unverifiable PID is
diagnostic-only and makes the row FAIL without a stop or signal. Every owned PID
must disappear. Concurrent owned PIDs or a surviving process is a
`no_orphan_daemons` failure. Retired sequential generations remain recorded so
every historical PID must still be gone; emergency SIGTERM/SIGKILL is
restricted to an exact trusted PID. The harness never uses `pgrep` or
`pgrep -f`. The second stop's shipped contract is JSON
`outcome=not_running` with exit 69, not exit zero.

## T0 checks

- `t0.agent.spawn_result` runs public `agent spawn` followed by `agent wait`
  against one finite fake-provider child segment. It requires distinct parent
  and child session/run identities, a correlated successful child terminal,
  positive durable terminal and ChildResult sequences, and the exact child
  report nonce. Its 288-second BudgetSum covers both command bounds and the
  runner's mandatory status-owned no-orphan cleanup. It reports correctness
  without publishing a timing measurement.
- `t0.run.jsonl_contract` runs `haider run -p x --provider fake --model
  fake-model --output jsonl --timeout 30s`. It requires first-line acceptance,
  a non-empty session, `head_seq == first envelope seq`, contiguous later
  sequence numbers, exactly one final `run_state` terminal with
  `terminal_kind=success`, exit zero, and a completed agent-message nonce from
  the sole finite fake-provider segment. The nonce proves the provider work was
  consumed; structurally a second request has no segment to consume.
- `t0.daemon.status_stop` requires `haider.observe.v1`, ready true, live positive
  PID, canonical profile/runtime/socket/PID paths, and bare `daemon.version`
  equal to the version parsed from `haider --version`. It then requires
  `stopped_cleanly`, the same PID with `process_exited=true`, PID disappearance,
  a second `not_running`/69 stop, and a no-spawn status probe that remains
  unavailable.
- `t0.run.exit_codes` records one evidence row for provider error, product
  timeout, max-time budget, a real client SIGINT, and missing credentials. The
  SIGINT row is a non-gating expected gap until the product defines signal
  semantics; every other row remains gating.
- `t0.budget.max_cost_binds_before_request` and
  `t0.budget.max_tokens_binds` count completed durable request-attempt facts,
  require a typed budget terminal before any below-bound exchange, and reserve
  one sole scripted segment in each of two sequential hermetic subcases: an
  above-bound one-request control and a below-bound zero-request probe. Each
  subcase proves its own no-orphan cleanup. Both checks carry
  `expected_fail_until=0.0.968` for the installed 0.0.967 baseline; this
  metadata never turns a failure into a pass.
- `t0.sessions.wait_ready_n` starts three sessions and proves both the exact
  positive barrier document and the distinction between readiness and turn
  quiescence: three current-format sessions are ready while state counts remain
  exactly two idle and one running.
- `t0.account.alias_selects` removes the daemon fake seam and owns two
  loopback-only stdlib OpenAI-compatible listeners, each with a distinct
  response. Hermetic dummy API-key environment variables create credential
  descriptors, and the listener implementation is kept below 150 lines.
- `t0.run.replay_resume_recover` compares durable replay against a read-only
  SQLite source projection, verifies replay consumes no provider request, then
  exercises finite typed resume and recovery commands.
- `t0.headless.input_required_is_typed` pins the installed 0.0.967 behavior:
  `request_input` is rejected as `no_human_available`, fed back to the fake
  provider, and the run completes instead of hanging.

All seven Step 2 rows set `timed=False`; they report correctness under load and
publish no timing verdict.

## T1 installed-artifact checks

T1 is the release-machine pack for the installed pair:

```sh
scripts/qa-gate/run.sh --tier t1 --bin-dir /usr/local/bin
```

- `t1.daemon.kill9_midturn` detaches a hanging fake-provider turn, kills only
  the status-owned daemon PID, proves a generation-incrementing respawn,
  requires a successful `recover --probe` receipt in `effect_unknown`, requires
  finite typed resume, then completes a fresh turn and clean stop. Installed
  0.0.967 currently reports `no_recovery/errored`;
  `expected_fail_until=0.0.968` records that defect without weakening the FAIL.
- `t1.daemon.lifecycle_triad` applies a 1,000 ms idle TTL to an autospawned
  daemon, requires exit within the derived 8,000 ms TTL/drain/observation
  window, then requires a different PID at generation +1 and a clean stop.
  Installed 0.0.967 keeps the same live PID/generation; the check carries the
  same strict expected-fail metadata.
- `t1.install.paths` runs `scripts/install.sh` with a scratch HOME and prefix,
  then proves the installed pair's version, readiness, status identity, clean
  stop, and PID disappearance. The runner bounds the installer as a whole;
  the installer's curl/wget calls currently have no internal timeout.
- `t1.store.previous_release_upgrade` downloads and checksum-pins the official
  v0.0.966 macOS arm64 pair, creates two real old sessions, owns the legacy
  daemon only through its profile PID file plus exact executable identity,
  opens the stopped profile with the binary under test, and compares its
  `PRAGMA user_version` and ordered `sqlite_master` rows with a fresh profile.
  A passing run publishes the stopped profile archive, checksum, and
  provenance manifest beside the report.
- `t1.turn.wall_budget` runs the stdlib loopback turn harness against one warm,
  settled daemon: five unreported warm-ups and 25 retained samples for each of
  the one-request and two-request tool shapes in ABBA order. Each sample gates
  on one typed terminal, contiguous journal sequence, exact provider request
  count (1/2), exactly one append-only local `process_exec` effect for the tool
  shape, durable Idle, and unchanged PID/generation. Timing is accepted
  only when all one-minute load snapshots at measured start/mid/end are below
  4; overload is `ENV_BLOCKED` and publishes no timing artifact. An accepted
  run publishes the raw samples, median/MAD, combined client+daemon CPU, peak
  RSS, binary/daemon/proxy/harness hashes, provider ledger, and exact-stop
  receipt.

LAW (TURN-WALL-1): on the pinned quiet runner, one warmed settled daemon plus
the vendored deterministic loopback provider must produce 25 valid attached
`haider run` samples per shape. The regression budgets are exactly 1.10 times
the accepted v0.0.968-main medians recorded in the check, while the owner
targets remain 40 ms for one physical provider request and 60 ms for exactly
two. MAD is diagnostic and relaxes neither ceiling. A wrong request count,
missing/duplicate tool effect, terminal/journal failure, PID/generation change,
sample failure, or unclean
exact stop is `FAIL`; a load rejection is
`measurement_accepted=false`/`ENV_BLOCKED`, never `PASS`.

CPU and peak RSS remain mandatory retained columns for a paired lane verdict,
but are not one-batch CI ceilings: repeated unchanged-binary batches showed
CPU movement larger than one batch's MAD. Peak RSS is sampled in page
granularity. The CI budget gates wall only; MAD never relaxes that budget or
the 40/60 ms owner targets.

The old step-4 `t1.tui.ladder` wrapper is intentionally absent: the five direct
hermetic `t0.tui.*` checks use the current PTY/RPC runner, while the legacy
14-demo + 2-live ladder remains independently wired in ship-gate. The old
suite-global process census is also omitted; it attributed unrelated installed
daemons, whereas every runnable check already ends with exact, status-owned
no-orphan enforcement.

## Normative report schema

`run.sh validate` and `run.sh diff` call the same `validate_report` function
used immediately before atomic report writing. That executable validator is the
authority for this table; additive keys are allowed.

| Location | Required shape |
| --- | --- |
| root | Object with `schema="haider.qa-gate.v1"`, non-empty `tier` and UTC `created_at_utc`. |
| `host` | Non-empty `hostname`, `platform`, and `python` strings. |
| `load` | Non-negative numeric `one_minute` from `os.getloadavg()[0]`; positive integer `logical_cpus`. |
| measurement | Boolean `measurement_accepted` and string-list `measurement_reasons`; accepted is true exactly when reasons are empty. Load greater than CPUs rejects root and every timed row. Warm-up failure also rejects timing rather than rewriting correctness. |
| `binary`, `daemon_binary` | Canonical path plus nullable lowercase SHA-256, exact `version_output`, and parsed bare `version`. Both pair members are hashed. |
| `daemon_version` | Nullable string collected from `status --json .daemon.version`; a normal passing run records the installed daemon's bare version. |
| `warmup` | Boolean `accepted`, integer `wall_ms`, non-empty `evidence_line`. Version executions and one isolated ready/start/stop happen before timed rows. |
| `checks[]` | Required `id`, `area`, aggregate `status`, non-empty `evidence[]`, non-negative integer `wall_ms`, string-list `artefacts`, boolean `timed`, and boolean-or-null `measurement_accepted`. The writer also records named budget parts, segment count, expected turns, and nullable `expected_fail_until`. |
| `checks[].evidence[]` | `label`, allowed `status`, non-empty single-line `evidence_line`, and string-list `artefacts`. The aggregate status precedence is `FAIL > ENV_BLOCKED > SKIP > PASS`. |
| `summary` | Exact integers `total`, `pass`, `fail`, `skip`, `env_blocked`, recomputed from the check array. |

JSON is UTF-8, sorted, indented, LF-terminated, finite-number-only, fsynced, and
atomically renamed into place.

## Diff and timing honesty

The runner records one-minute load and logical CPUs once before execution. When
load exceeds CPUs, every timed check stores `measurement_accepted=false`, but
its correctness evidence and aggregate status are unchanged.

`run.sh diff previous current` validates both reports, prints added/removed
checks and every status flip, then considers wall time only where both rows were
timed and accepted. Since each report stores one wall value per check, the MAD
is precisely defined across the population of matched signed deltas:

```text
delta_i = current.wall_ms_i - previous.wall_ms_i
MAD = median(abs(delta_i - median(delta)))
threshold_i = max(3 * MAD, 0.20 * previous.wall_ms_i)
```

A `WALL` line is printed when `abs(delta_i) > threshold_i`. Diff exits 1 for an
added, removed, or status-flipped check; wall-only diagnostics do not change its
exit code.
