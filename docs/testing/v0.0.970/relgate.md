# v0.0.970 relgate

## Claim audit (before implementation)

Audited starting tree `471b9d680610b62c4cdd4a8be7b6ee7faf3959d3`.
Citations below use final working-file lines; original ship-gate job lines
were 20, 45, 65, and 103. Read the supplied lane briefs and turnperf/turnperf2
evidence as historical context, not fresh performance measurements. These
supplied files are excluded from the deliverable.

| Claim | Audit and evidence |
| --- | --- |
| Release calls ship-gate and build needs it | Correct: `.github/workflows/release.yml:19`, `:23`, `:25`, `:26`. |
| Ship-gate has three macOS jobs and does not consult ci/xplat | Core gap correct, job count drifted: four existing jobs, all macOS. Probe `.github/workflows/ship-gate.yml:39`, `:42`; render `:65`, `:68`; daemon footprint `:86`, `:89`; advisory client footprint `:125`, `:133`, `:134`. Before this patch none queried another workflow. The new evidence job is Linux. |
| Release matrix only compiles | Overstated: it compiles (`.github/workflows/release.yml:80`, `:106`, `:111`) and runs binary self-test/version smoke (`:174`) and stock-environment startup smoke (`:205`). It does not run workspace clippy/tests. |
| Windows/Linux clippy+tests live only in xplat | Full platform suites: correct (`.github/workflows/xplat.yml:24`, `:33`, `:59`, `:67`, `:73`). Exclusivity of Linux tests: wrong; ci also runs Linux X11 E2E (`.github/workflows/ci.yml:95`, `:111`). |
| A v* tag does not create xplat/ci evidence | Correct: both `.github/workflows/xplat.yml:2` and `.github/workflows/ci.yml:2` restrict push to branches `["**"]`, plus PR and workflow_dispatch. Release separately accepts tags (`.github/workflows/release.yml:2`). Existing branch, PR, or manually dispatched runs may supply the same head_sha; branch pushes are not the only possible source. |

GitHub documents that specifying only branches excludes tag pushes, and a
called workflow uses the caller's context. See [workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#onpushbranchestagsbranches-ignoretags-ignore)
and [reusable workflow reference](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations).

The historical failure interval was inspected with read-only `gh` queries:
last listed wave-970 xplat success [33711525590](https://github.com/Rizzist/haider-agent/actions/runs/33711525590)
started at 2026-09-03 03:29:11 UTC; the next run [33716640454](https://github.com/Rizzist/haider-agent/actions/runs/33716640454)
started at 04:52:32 UTC and failed. Later listed runs through September 5 were failure or
cancelled, including [33980468392](https://github.com/Rizzist/haider-agent/actions/runs/33980468392)
on `f211be0e9fb6ca960d0fa73e0dbc970f2a04fb37`. The first failure was the
unsafe-count guard, so the entire interval cannot be attributed to platform bugs.

The decisive same-SHA pair is successful [ci 33953072908](https://github.com/Rizzist/haider-agent/actions/runs/33953072908)
versus failed [xplat 33953072907](https://github.com/Rizzist/haider-agent/actions/runs/33953072907),
both `7431f8e6e9500729362cc4eb3cfb2bbc62cf462a`. This also appears at
`docs/testing/v0.0.970/xplatfix.md:19` and `:22`. Hosted macOS lint/tests passed
while Windows check/clippy/tests and Linux clippy failed. Sustained *local*
macOS green is owner-reported; the hosted discrepancy was directly verified.

The 0.0.935 Chocolatey incident is the same blind-spot class by inspection:
`docs/testing/v0.0.970/chocofix.md:19` describes moderator-observed metadata;
`:26` describes Windows CRLF eligibility; `:35` records a reproduced LF/CRLF
regex split; `:47` explicitly limits forensic claims about that historical
runner. This lane did not rerun the PowerShell reproduction or alter publishing.

## Enforced policy

The candidate is the caller's `github.sha`, explicitly supplied to the shared
resolver by `.github/workflows/ship-gate.yml:36`. It is never inferred from the
latest branch run. The resolver queries each workflow file's runs with
`head_sha`, paginates, and independently filters returned SHA values
(`scripts/release/require-evidence.sh:32`, `:39`). Evidence from any event/ref
is eligible only when the SHA matches exactly.

| Evidence for each required workflow | Result |
| --- | --- |
| Any completed success for exact SHA | Pass that workflow; an existing success takes precedence over other runs. |
| No success, but queued/in_progress/requested/waiting/pending run | Poll; pass only on completed success, fail on terminal nonsuccess or timeout. |
| No run for exact SHA | Resolve dispatch ref to candidate; dispatch that workflow once, then poll through registration delay. |
| Ref missing, moved, or cannot be resolved | Fail closed with workflow and SHA. |
| Failure/cancelled/timed_out/skipped/neutral/action_required or unknown state, with no success or active run | Fail closed with conclusion/status, workflow and SHA. |
| Dispatch forbidden, API error, malformed response, exhausted polls | Fail closed with one-line reason naming workflow and SHA. |

Both xplat-check (`xplat.yml`) and ci (`ci.yml`) must pass. Each has at most
120 polls at 60-second intervals; test overrides can only shorten this budget.
The outer job budget is 2 × 120 × 60 seconds = 240 minutes, plus 5 minutes for
setup/API overhead (`.github/workflows/ship-gate.yml:26`). Missing evidence is
dispatched on `GITHUB_REF_NAME`; a ref that advances during dispatch cannot
satisfy the fixed-SHA query. No evidence failure is advisory.

All four existing jobs now need evidence (`.github/workflows/ship-gate.yml:40`,
`:66`, `:87`, `:126`). Release still has exactly one build prerequisite:
ship-gate. Actions write is limited to the reusable caller job
(`.github/workflows/release.yml:20`) and the evidence job
(`.github/workflows/ship-gate.yml:28`); other ship-gate jobs have contents read.
The preexisting advisory client-footprint calibration policy is preserved.

`scripts/ship-970.sh` did not exist in this tree or its available history.
The new local pregate resolves the candidate commit and executes this same
resolver (`scripts/ship-970.sh:7`, `:9`). Usage before tagging:
`bash scripts/ship-970.sh <published-candidate-ref> [candidate-sha]`.
It performs no tag creation or publication. Push-to-main and reusable/tag
invocations use identical evidence logic. CI's existing script-test area now
runs the stubbed policy suite (`.github/workflows/ci.yml:56`).

## Executed versus inspected

- Executed: 53 deterministic shell cases, covering all policy branches for
  both workflow names, pagination/older success, wrong SHA, registration delay,
  exactly one dispatch, terminal nonsuccesses, API failures during polling,
  ref mismatch, both timeout paths, main-ref dispatch, and local pregate
  propagation of HEAD despite a wrong ambient GITHUB_SHA. Exact exit codes,
  reason lines, poll counts and dispatch counts are asserted.
- Executed: Bash syntax checks and unsafe-count guard (production 189, tests 20).
- Executed: PyYAML parsing of release, ship-gate, ci, and unchanged xplat;
  actionlint was unavailable. PyYAML was installed in `/tmp/relgate-yaml-env`.
- Inspected: GitHub workflow dependency/context/permission behavior; Windows
  and Linux test definitions. No Windows/Linux suites were executed locally.
- **Not proved locally: the live gh dispatch path.** Dispatch success and
  forbidden responses were stubbed. No real workflow was dispatched, so live
  token authorization, runner scheduling, and dispatch-to-run registration
  remain unverified.
- Executed: fresh `cargo build -p haider-daemond -p haider-cli --locked`
  passed in 8m33s under ENV LAW; `haiderd` is 201,719,920 bytes (>10 MiB).
- Executed: `cargo run -q -p xtask --target-dir /tmp/relgate-xtask-target --
  test-count --update` reports 5,027, equal to the merged upstream baseline.
  The initial lane baseline was 4,997; the upstream forward brings 5,027.
  Shell cases do not change the Rust test count.
- Executed: `cargo clippy --workspace --tests -- -D warnings` passed (exit 0,
  7m46s) under ENV LAW, including `HAIDER_TEST_SIBLINGS_PREBUILT=1`.
- Prior-tree full workspace test: `cargo test -q --workspace --no-fail-fast`
  completed with exit 101, four failures across two targets. Both launcher
  failures reproduced when rerunning `haider-client --test client_tests`
  (16 passed, 2 failed). The two daemon budget cases passed a scoped rerun
  (`run_budget_tests::subturn_`, 2 passed); this does not turn the failed full
  run green. Final merged-tree results are recorded below.

## Merge-forward and delivery

`git fetch origin wave-970` failed because the sandbox denies writing shared
Git `FETCH_HEAD`. `git merge --no-commit origin/wave-970` then failed writing
`ORIG_HEAD.lock`. The available remote-tracking ref was
`f211be0e9fb6ca960d0fa73e0dbc970f2a04fb37`, a descendant of lane HEAD.
Applied its binary diff to the allowed working files before the Rust build;
all 545 upstream paths matched that commit byte-for-byte afterward. No
conflict/golden was hand-merged. The working content includes upstream, but
the Git merge/branch metadata could not be recorded. The original lane HEAD
therefore remains `471b9d68`; upstream files must be preserved when landing.
An explicit seven-file `git add` also failed creating `index.lock` (Operation
not permitted), so the requested lane commit cannot be created in this
sandbox. No files were staged, no commit/trailer was added, and nothing pushed.
`/tmp/relgate-lane.patch` contains only this lane's seven deliverable files;
the upstream merge-forward patch is separate at `/tmp/relgate-upstream.patch`.

During validation the available upstream ref advanced to
`6c6164c93644ffcd9ef3c8c3c65fc34432d7fb0a` (docsync merge). A second fetch/merge
attempt hit the same metadata restrictions. Applied its 15-path forward delta
and verified every path byte-for-byte before starting the final gate. That
delta is `/tmp/relgate-upstream-2.patch`. The earlier Rust results above refer
to the first, `f211be0e`, snapshot; they are not claimed for the final snapshot.
After the final workspace test, all 553 upstream-affected paths still matched
`6c6164c9` byte-for-byte, including added files and regenerated fixtures.
The second forward retains the default macOS byte pin 5,670 -> 5,670, adds the
upstream full-manifest pin 20,770, and moves the test-count baseline to 5,029.
The named fixture is regenerated through `UPDATE_FIXTURES=1` with an exact
test filter before the final workspace run; no golden is hand-edited.

Final-snapshot preparation passed: fresh siblings (4m31s, daemon 201,719,920
bytes), all 53 shell cases, unsafe counts 189/20, named fixture regeneration
(one test passed; the resulting file equals upstream), and the named byte-pin
test (one passed). Measured default instruct-pipe bytes remain 5,670, and
full-manifest bytes are 20,770. `xtask test-count --update` returned 5,029;
there is no lane-specific baseline delta from `6c6164c9`.

The **final full workspace test passed (exit 0)** on this second snapshot:
`cargo test -q --workspace --no-fail-fast`, with all ENV LAW variables plus
fresh `HAIDER_TEST_SIBLINGS_PREBUILT=1`. Both earlier failing targets passed
in this full run; no test was skipped, ignored, weakened, or platform-gated
by this lane. Existing repository ignores remain unchanged. This supersedes
the prior-tree failure for local validation, without claiming a relgate code
fix for the earlier launcher/provider timing failures.

Execution context: the external disk governor logged pauses of this lane for
14m30s and 2m03s during the work. These are recorded as context, not a proven
cause of any assertion failure. Logs are `/tmp/relgate-workspace-test.log`
(first snapshot), `/tmp/relgate-client-rerun.log`,
`/tmp/relgate-budget-rerun.log`, and `/tmp/relgate-final-*.log` (final snapshot).
Final-snapshot `cargo clippy --workspace --tests -- -D warnings` passed (exit 0,
3m10s). All steps in `/tmp/relgate-final-gate.sh` returned 0, under ENV LAW;
disk was checked before each Cargo invocation. The three changed workflows
and unchanged xplat parsed successfully with PyYAML; actionlint was unavailable.

## Disposition

The code review and final local gate pass. **NO_SHIP for lane delivery:** the
sandbox prevented recording the merge and the explicitly requested lane
commit. The live gh dispatch path remains unproved locally, as stated above.
The seven-file patch is ready at `/tmp/relgate-lane.patch`, based on
`6c6164c9`; suggested commit subject: `Require exact-SHA CI evidence before release`.
No commit or push was performed, and the supplied lane briefs/turnperf evidence
are excluded from this patch.

## Registry walk

Read the CI error registry through #98 (including duplicate #94/#95 numbering).
For this lane's workflow/shell delta, classes below are checked by inspection;
upstream Rust changes are exercised by the required full workspace commands.

| Classes | Result |
| --- | --- |
| #1–19, #22–30, #34–43, #45–60, #62–63, #65–69, #71–76 | Checked: none introduced; no Rust, protocol, dependency, platform implementation, fixture, or UI changes in this lane. |
| #20, #85–88 | Full workspace tests/clippy-with-tests and test-count required despite workflow-only scope; results recorded below. |
| #21, #54 correction, #64, #74, #81 | ENV LAW used; fresh siblings required before prebuilt mode, daemon must exceed 10 MiB. |
| #31–32, #33, #79–80 | Checked: Android/publish/test-runner behavior and existing footprint calibration left intact. |
| #61, #70, #77–78 | Fixed: executable exact-SHA evidence policy plus regression suite; dispatch only when evidence is absent, once per workflow. |
| #44 | Sandbox execution limits reported rather than claimed green. |
| #82–84, #90, #92, #94/#95 disk variants, #96, #98 | Resource/load failure classes considered; disk checked before each build, no broad process manipulation. |
| #89, #91 | Upstream content applied and all 545 affected paths verified; metadata limitation stated explicitly. |
| #93 | Checked: edits use patches; no BSD-sed whitespace assumptions. |
| #94 deadline variant, #95 keepalive variant | Poll/job budgets derived above; no persistent negotiated application connection held during sleeps. |
| #97 | Explicit lane deliverables only; no binaries, supplied briefs, or turnperf evidence staged. |

Independent verifier: no code defects; independently passed all 53 cases,
Bash syntax checks, and the four PyYAML parses. One report-only finding corrected
run start timestamps previously labeled as failure times. Under the requested
code/test/verdict metric this is findings=1, real=0, noise=1: rejected as a
functional/verdict finding because no code, test, or SHIP decision changed;
the factual wording was corrected.
