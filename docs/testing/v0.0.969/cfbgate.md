# Lane cfbgate — v0.0.969 client-footprint ship-gate hotfix

Date: 2026-09-02  
Branch/base: `lane-969-cfbgate` / `3a653b1` (`v0.0.969`)  
Scope: Python harness, QA fixture/tests, workflow, and this evidence file. No
product Rust source was changed and no Cargo build was run.

## Outcome

The client-footprint job is advisory until it records its first CI pass. The
three surfaces and their budgets remain unchanged, and the other three
ship-gate jobs remain blocking.

The run harness now removes the 30-second deadline race, cannot miss a terminal
prefetched by Python's text buffering, uses an exact IPv4 loopback route under a
proxy-scrubbed environment, and preserves useful evidence on every run failure.
A typed timeout/provider-error terminal ends the observation wait, but it does
**not** pass the footprint surface: only `terminal_kind=success` reaches settled
measurement and budget enforcement.

The post-change CI result is not yet available, so the original runner failure
is not attributed to one cause. Static evidence rules out both suspected product
causes:

- Custom OpenAI-compatible inference uses `compatible_transport`, whose shared
  client builder already calls `.no_proxy()`
  (`crates/haider-provider/src/openai.rs:165-179,1215-1227`). Proxy variables
  therefore cannot redirect this fixture's product request on this base.
- `VaultProvision::PlatformDefault` resolves to a profile-local `FileVault`
  (`crates/haider-daemon/src/accounts.rs:10583-10598`). The file-vault module
  explicitly records that the macOS Keychain default was retired
  (`crates/haider-accounts/src/file_vault.rs:1-17`), and the fixture's raw alias
  is supported by the legacy profile-vault fallback
  (`crates/haider-daemon/src/profile_vault.rs:67-75`).
- The QA stub already bound and advertised `127.0.0.1`. The hotfix pins that
  invariant and proves reachability from a separate sanitized subprocess before
  spawning `haider run`.

No product fix is warranted without contrary CI diagnostics.

## Changes

`scripts/perf/client-footprint-budget.py` now:

- removes `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and all three lowercase
  variants; sets both `NO_PROXY` and `no_proxy` to
  `127.0.0.1,localhost`;
- rejects any stub URL not advertised as `http://127.0.0.1:<port>/...`, then
  runs an isolated Python subprocess GET against `/v1/models` with the same
  sanitized environment and stores `stub-reachability.json`;
- passes `--timeout 45s` to the product and derives the outer terminal deadline
  as `2 * 45s = 90s`;
- reads raw stdout bytes and splits complete JSONL records itself. This removes
  the `select()` + `TextIOWrapper.readline()` prefetch race and partial-line
  blocking risk. Child stderr is written directly to the artifact rather than
  left in an undrained pipe;
- treats any typed terminal as “terminal seen,” writes `run.jsonl`, then
  separately requires success. Typed timeout/provider errors therefore produce
  diagnostics and still fail the budget step;
- on timeout, exit-before-terminal, typed failure, or other run failure, writes
  full `run.stdout`, `run.stderr`, `stub-requests.json`, `failure.json`, the
  stable/per-launch daemon logs, and `diagnostic-excerpt.txt`, and prints the
  bounded excerpt to the job log before the throwaway profile is removed.

`scripts/qa-gate/gate/openai_stub.py` exposes a locked snapshot of all stub
requests, including the reachability GET. The success law remains exactly one
chat-completions POST, not one total HTTP request.

`.github/workflows/ship-gate.yml` sets job-level
`continue-on-error: true` only on `client-footprint`, with the registry #80 and
this document named in the comment. The existing `if: always()` artifact upload
is unchanged. `behaviour`, `render-benchmark`, and `daemon-footprint` have no
advisory flag and continue to gate `release.yml`'s five-target build matrix.

## Named tests

The new `scripts/qa-gate/tests/test_client_footprint_budget.py` covers:

- upper/lowercase proxy removal and exact loopback bypass variables;
- exact IPv4 stub advertisement and a real subprocess reachability GET;
- rejection of `localhost` and IPv6 URL spelling;
- complete exit-before-terminal diagnostics, including both daemon log forms
  and a bounded job-log excerpt;
- two JSONL records coalesced into one OS write, which catches the former text
  prefetch race;
- typed provider-error terminal seen-but-failed semantics;
- exact 45-second product timeout and derived 90-second observation deadline.

Verification output:

```text
$ TMPDIR=/private/tmp python3 scripts/qa-gate/runner.py test
Ran 61 tests in 0.400s

OK

$ python3 scripts/perf/client-footprint-budget.py --self-test
client-footprint self-test: PASS footprint=81992 threads=1

$ bash scripts/check-unsafe-counts.sh
unsafe-count gate: PASS production=189 test=16
```

`python3 -m py_compile` for the harness, stub, and new test passed.
Ruby's YAML parser accepted `ship-gate.yml`; a static pin confirmed exactly one
job-level advisory flag and all three unchanged surface/budget pairs.
`git diff --check` passed. No Rust test or Cargo build was needed for this light
lane.

An independent verifier reviewed the current diff and reran the light gates. It
returned **SHIP** with no blocker; its only caveat was that failures before stub
creation/wire-fixture setup cannot have child/stub/daemon execution diagnostics,
which is outside the requested timeout/exit/typed-terminal paths.

## Real release-binary surfaces

All three commands used the read-only v0.0.969 binaries at
`/Users/rizzist/haider-run/wt-965/target/release/{haider,haiderd}`, the full
60-second settle, and the checked-in budgets. Verbatim output follows.

```text
$ python3 scripts/perf/client-footprint-budget.py --haider /Users/rizzist/haider-run/wt-965/target/release/haider --surface status-post-command --output /private/tmp/cfbgate-local/status --budget-bytes 2703783
status-post-command run=1 footprint=2392448 cpu_us=86499 threads=1 load=1.33/2.06
client-footprint: vmmap -summary failed exit=255; diagnostic=/private/tmp/cfbgate-local/status/run-1/vmmap-summary.txt

$ python3 scripts/perf/client-footprint-budget.py --haider /Users/rizzist/haider-run/wt-965/target/release/haider --surface run-post-command --output /private/tmp/cfbgate-local/run --budget-bytes 3406683
run-post-command run=1 footprint=3129752 cpu_us=113476 threads=1 load=1.84/1.38
client-footprint: vmmap -summary failed exit=255; diagnostic=/private/tmp/cfbgate-local/run/run-1/vmmap-summary.txt

$ python3 scripts/perf/client-footprint-budget.py --haider /Users/rizzist/haider-run/wt-965/target/release/haider --surface tui-demo-sixel --output /private/tmp/cfbgate-local/tui-sixel --budget-bytes 6110043
tui-demo-sixel run=1 footprint=5636504 cpu_us=2550684 threads=4 load=1.35/1.57
client-footprint: vmmap -summary failed exit=255; diagnostic=/private/tmp/cfbgate-local/tui-sixel/run-1/vmmap-summary.txt
```

This managed sandbox denied `vmmap` task-port access, so all three strict runs
were correctly rejected as measurements after the settled `proc_pid_rusage`
sample. They are diagnostic rather than accepted footprint passes. Each observed
footprint was below its unchanged budget by 311,335 B, 276,931 B, and 473,539 B
respectively. The run artifact independently proves 25 JSONL rows, exactly one
successful terminal at seq 24, an empty child stderr, and a successful subprocess
GET to the exact `127.0.0.1` stub. This is the failing CI surface that previously
produced no terminal or artifact data.

## Citation audit

- The lane brief's `3a653b1` / v0.0.969 base is correct. The common file's
  `8952219` / v0.0.967-candidate base is stale.
- The two cited pre-hotfix run failures and their 30-second message are correct.
  The run timeout/deadline citations drift after this patch by design.
- “Whole 4-platform build” describes four OS/architecture classes, but the
  current `release.yml` matrix has five target entries: two macOS, two Linux,
  and one Windows.
- Registry #80 remains applicable: a typed run terminal is not the later
  aggregate Session Idle. This harness stops its observation wait at the typed
  run terminal, then independently requires success before settling.

## CI error registry walk

- #10/#19: Python compilation, the named unit suite, and `git diff --check`
  passed; no dead Rust helper was added.
- #41: the QA suite's default long macOS temp root correctly tripped the
  pre-existing 64-byte path guard. Re-running with the required short
  `TMPDIR=/private/tmp` passed all 61 tests.
- #64: the exercised `haiderd` is 52,341,136 B, above the 10 MiB release guard.
- #71: the real v0.0.969 release artifacts were exercised end to end on all
  three surfaces, not inferred from mocks.
- #72/#74: discovery is disabled only inside the hermetic fixture; every sample
  uses and removes a throwaway profile.
- #77: the unsafe-count guard, Python syntax checks, harness self-test, and QA
  unit suite all passed. The workflow retains the unsafe guard before builds.
- #80: terminal observation and aggregate session settlement remain distinct;
  typed failure is observed but never accepted as a footprint pass.
- #94: the terminal wait is documented arithmetic (`2 * 45s = 90s`); the stub
  probe outer deadline is likewise `2 * 5s = 10s`.
- #95: no new negotiated-connection wait was added to the product; the outer
  script observes the child process only.
- #96: `vmmap` denial remains a measurement rejection, never a rewritten pass.
- The supplied `LANE-*`, `turnperf/`, and `turnperf2/` evidence remains
  unmodified and uncommitted.

SHIP
