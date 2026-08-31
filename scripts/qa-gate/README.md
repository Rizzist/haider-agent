# Haider QA gate

This is a Python 3.11, standard-library-only functional gate for an **installed**
`haider` and sibling `haiderd`. It never runs Cargo, never reads a real Haider
profile, and the T0 checks use only the daemon-owned fake provider. The entry
point pins a short temporary root before Python starts so macOS Unix socket paths
cannot silently fall back outside the check's runtime root.

## Commands

```sh
scripts/qa-gate/run.sh --tier t0 --bin-dir /usr/local/bin
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
| `needs` | Tuple/list drawn from `binary`, `daemon`, `pty`, `network:none`, or `fixture:<relative-path>`. A known unavailable need produces `ENV_BLOCKED` without calling `run`. |
| `script` | List of fake-provider step objects. It is compact-JSON encoded into `HAIDER_TEST_FAKE_PROVIDER` before any process starts. |
| `turns_expected` | Explicit non-negative integer required by the segment law. This field supplements the base check contract because the runner cannot enforce that law without it. |
| `budget` | A `BudgetSum` of at least two positive named `BudgetPart` values, each with a source. An `int`, `float`, lone part, or other literal-only deadline is rejected while loading all checks, before any spawn. |
| `timed` | Boolean. Correctness always stands; an overloaded host rejects only the published timing. |
| `expected_fail_until` | Optional non-empty version string recorded in the report. It documents a known-open release defect but never converts FAIL to PASS. |
| `run(ctx)` | Returns a non-empty `list[Evidence]`. Exceptions become a diagnostic runner `FAIL`, followed by mandatory daemon cleanup. |

`Evidence(label, status, evidence_line, artefacts)` accepts only `PASS`, `FAIL`,
`SKIP`, or `ENV_BLOCKED`. The label and evidence line must be non-empty, and the
evidence line must contain no CR/LF. This keeps every human verdict truthful and
single-line while the JSON preserves all individual evidence rows.

Fake-provider segments end at exactly these shipped step names:
`finish`, `error`, `error_presented`, `hang`, `premature_eof`,
`error_with_retryability`, and `malformed_frame`. All checks are loaded first;
the runner refuses `turns_expected > segment_count` before creating a context.
One daemon and one consumptive script belong to one check—never to a tier or a
neighbouring check.

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
must disappear. Multiple owned PIDs or a surviving process is a
`no_orphan_daemons` failure; emergency SIGTERM/SIGKILL is restricted to an exact
trusted PID. The harness never uses `pgrep` or `pgrep -f`. The second stop's shipped contract is JSON
`outcome=not_running` with exit 69, not exit zero.

## T0 checks

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
