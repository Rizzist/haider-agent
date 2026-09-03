# Lane cfbcal — runner calibration for client-footprint budgets

Date: 2026-09-02  
Base: `e3fc3f5` (`v0.0.969`)  
Scope: client-footprint Python harness, its QA tests, ship-gate workflow, and
this evidence file. No product source changed.

## Outcome

The advisory ship-gate job now exercises the existing strict calibration path
for all three client surfaces. Each surface records exactly five independent,
60-second-settled measurements on `macos-15`; all samples require load below
four and successful `vmmap -summary` evidence. A failure on one surface is
collected rather than allowing Bash to skip the remaining surfaces. The final
step still fails the advisory job if any surface failed.

The uploaded artifact is named
`client-footprint-<os>-<arch>-<github-run-id>-<attempt>` and contains
`status/summary.json`, `run/summary.json`, and `tui-sixel/summary.json` plus all
five samples and diagnostics beneath each surface. Every summary identifies the
GitHub run/attempt, SHA, runner OS/architecture, calibration mode, median, MAD,
and `ceil(median * 1.10)` candidate budget. Missing artifacts are now an error;
retention is 90 days.

The repository does not yet have the requested all-surface N=5 runner artifact:
the old sequential budget step stopped at the run failure before TUI, and the
fixed run surface has only two accepted runner observations. Consequently the
status budget is based on a recovered N=5 runner aggregate, the run budget is a
runner bootstrap, and the TUI budget remains the explicitly provisional developer-Mac
value. This lane must remain advisory and is **NO_SHIP** until a post-change run
supplies the missing N=5 run/TUI medians and the workflow budgets are replaced
from that artifact.

## Recovered GitHub runner evidence

All values below are accepted `proc_pid_rusage` readings with `vmmap_exit=0`.
The status set spans the v0.0.969 pre-hotfix and script-only hotfix SHAs; no
product Rust changed between those SHAs.

| Surface | Accepted runner values | Median used | Workflow budget | Provenance |
| --- | ---: | ---: | ---: | --- |
| status | 2,671,488; 2,851,840; 2,474,880; 2,687,872; 2,458,432 B | **2,671,488 B (N=5)** | **2,938,637 B** = `ceil(median * 1.10)` | Runs `33597163437`, `33597169862` (two attempts), `33617303520`, `33617313643`; artifacts `9835522465`, `9836182758`, `9838341052`, `9844026664`, `9844531667` |
| headless run | 3,458,176; 3,441,728 B | 3,449,952 B (N=2; insufficient for calibration) | **3,794,948 B** = `ceil(median * 1.10)` | Main run `33617303520`, artifact `9844026664`; retag run `33617313643`, artifact `9844531667` |
| Sixel TUI | no runner observation | unavailable | **6,110,043 B**, unchanged provisional local value | The preceding run budget failure prevented this command from starting |

The retag evidence in run `33617313643`, job `100206047952`, exactly confirms
the lane brief: the repaired fixture emitted `terminal_seq=23`, made one
provider request, recorded 3,441,728 B, and failed only the old 3,406,683 B
budget. Same-SHA main run `33617303520`, job `100206011257`, recorded 3,458,176
B. Both uploaded complete run summaries. The brief's 3,146,136 B local value is
not present in a checked-in artifact and is treated as owner-supplied context.

The status budget therefore has exactly 10% median headroom. The run bootstrap
has exactly 10% headroom over the available N=2 runner median; because N=2 is
not the required calibration sample, it remains provisional. No runner-headroom
claim is made for TUI.

## Calibration and enforcement behavior

`scripts/perf/client-footprint-budget.py` now labels summaries as `guard` or
`calibration`, records the runner context supplied by GitHub Actions, and derives
the reported candidate budget from the physical-footprint median rather than
the maximum. Calibration accepts exactly `--runs 5`; it continues to require a
settle of at least 60 seconds, load at most four, and successful `vmmap` for
every sample.

The configured budget remains an upper bound on every individual sample. Thus
an N=5 calibration artifact is still written before a tail sample at or above
the configured budget makes that surface return failure. This preserves the
ship gate while making calibration evidence usable for a follow-up adjustment.

## Advisory-to-blocking registry note

Registry #80 remains applicable: the run surface ends its observation at the
typed run terminal and separately requires terminal success; it does not infer
run completion from later aggregate session settlement.

TODO dated 2026-09-02: after the calibrated budgets produce green
`client-footprint` job conclusions on **three consecutive, distinct main push
runs**, land a follow-up that records those three run/job IDs and removes the
job-level `continue-on-error: true`. Tag runs and reusable-workflow invocations
do not count. The job conclusion must be checked directly because an advisory
job failure can leave the overall workflow conclusion green.

## Named tests

`scripts/qa-gate/tests/test_client_footprint_budget.py` now pins:

- rejection of calibration at the default N=1, N=4, and N=6, and acceptance at
  exactly N=5;
- a mocked complete five-sample calibration, including all sample artifacts,
  runner/run provenance, the physical-footprint median, and exact 10% headroom;
- the ship-gate wiring for status, headless run, and Sixel TUI with
  `--calibrate --runs 5`.

## Verification

- `TMPDIR=/private/tmp python3 scripts/qa-gate/runner.py test` — 64 passed,
  zero failures or ignored tests.
- `python3 -m py_compile scripts/perf/client-footprint-budget.py
  scripts/qa-gate/tests/test_client_footprint_budget.py` — pass.
- `python3 scripts/perf/client-footprint-budget.py --self-test` — pass; a
  one-thread child and positive physical footprint were observed.
- `bash scripts/check-unsafe-counts.sh` — pass, production 189 / test 16.
- Ruby's YAML loader accepted `.github/workflows/ship-gate.yml` with aliases;
  `actionlint` is unavailable in this worktree.
- `git diff --check` — pass.

## Citation audit and CI registry walk

- The brief contains no inherited `file:line` citation to drift. Its v0.0.969
  retag values, workflow path, typed terminal, and provider-request count are
  correct against GitHub artifacts.
- `LANE-COMMON.md` says base `8952219`; that has drifted. This worktree is based
  on `e3fc3f5`, the v0.0.969 retag hotfix merge requested by the brief.
- The old budget provenance is slightly more precise than the brief's shorthand:
  `docs/testing/v0.0.969/memclient.md` calls the developer-Mac N=5 values
  rejected diagnostics because local sandboxing denied `vmmap`, and calls their
  max-derived budgets provisional.
- #64 remains in the workflow: the release `haiderd` must exceed 10 MiB.
- #71/#74: each calibration sample drives the real release client with a fresh,
  removed profile; no product behavior is mocked by the workflow.
- #77/#96: the harness self-test precedes measurement and every accepted sample
  still requires `vmmap`; missing evidence cannot become a pass.
- #80: terminal observation and session settlement remain separate.
- #94: existing deadlines remain documented arithmetic (`2 * 45s` for the run
  terminal and `2 * 5s` for the stub probe).
- No OAuth file, Rust product source, or parallel-lane-owned file changed. The
  supplied `LANE-*`, `turnperf/`, and `turnperf2/` evidence remains unmodified
  and uncommitted.

NO_SHIP
