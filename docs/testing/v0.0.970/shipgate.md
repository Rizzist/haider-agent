# v0.0.970 shipgate — owner correction

**NO_SHIP.** The source/test changes are complete and the full Rust gate passes. The local ladder is 14/16 because process enumeration is unavailable; strict local footprint acceptance is blocked by host load, with an independent vmmap task-port restriction also demonstrated.

## Scope and merge-forward

Base and freshly fetched `origin/wave-970`: `f724335daddff364b01da4548b45e67fcfd12277`. The original worktree cannot write its external Git metadata (`FETCH_HEAD` and `ORIG_HEAD.lock`: Operation not permitted). The existing writable clone at `/private/tmp/haider-shipgate-repository` fetched `origin wave-970` successfully and `git merge --no-commit origin/wave-970` reported `Already up to date` before the final gate. Deliverable: branch `lane-970-shipgate-bundle` in that writable clone, packaged as `tmp/shipgate/shipgate-owner-correction.bundle`, without a trailer or push. The lane instruction files and turnperf/turnperf2 reference evidence are excluded.

The previous attempt restored first-request `request_input` exposure and compressed delegation prose to fit a compensating byte pin. All five affected product/test files were restored before implementing the owner correction. No request-input card is restored. The only subsequent Rust change corrects model-facing documentation, preserving schemas and behavior.

## Probe contract

`pty-probe-live.py` now requires exactly one injected tool result and zero session-scoped `menu_opened`, `menu_answered`, and `menu_resolutions`. Live peers and cold reconstruction must show the continuation and no card. Presence of continuation on forced full repaints prevents an empty screen from satisfying absence. Captured streams also reject transient card sentinels. The advertised ceiling remains unchanged; the injected result is not evidence of a user answer or a chosen option.

The probe now also pins HOME, XDG, and runtime roots to its throwaway directory and scrubs inherited Haider/credential overrides. A local pre-fix run failed daemon lockdown against the real home; after isolation the session checks execute successfully.

No-ERRORED, no `sk-` key material, exactly-once continuation, hermetic profiles, daemon-outlives-TUI (R8), and clean teardown are retained. Credential scanning now covers all three terminal streams. `/voice` is independently demo-only (`app.rs:14148`, the `!self.mode.fabricates_locally()` guard), so its live-mode explanatory flash remains required; its assertion was not relaxed. The client contract and model manual distinguish automatic completion from explicit interactive compatibility.

## Client footprint: calibration, not an unverified allocator change

The original release-runner artifact is `client-footprint-macOS-ARM64-34045715250-1`, artifact ID `9994242265`, run `34045715250`, attempt 1, SHA `f724335daddff364b01da4548b45e67fcfd12277`; its zip SHA256 is `4545e1589a2c5b17b498590955db3b865ff23081b76dd59b799c765c15293bc7`. The downloaded evidence directory is named `tmp/shipgate/ci-footprint`, but **these are release-profile measurements**, not `--profile ci`: `footprint-job.log` records `cargo build --release -p haider-cli -p haider-daemond -p haider-tui-exe --locked` at 16:37:31 UTC and the harness's `--haider target/release/haider` at 17:36:28 UTC. Runner: macOS 15.7.9 ARM64, image `20260829.0321.1`.

All three sets were produced by the real strict `--calibrate --runs 5` path with a 60-second settle, load below four before spawn and before reading, and `vmmap_exit=0` for all 15 samples. That completed calibration is the basis for the new limits. Allocator and control-flow changes were reverted, but model-facing prose changed after the artifact SHA; this is not post-change acceptance evidence. The job failed the prior run/Sixel limits after writing all evidence; measurement acceptance and budget acceptance are separate.

| Surface | Accepted physical-footprint bytes, in sample order | Median | Old → new budget |
| --- | --- | ---: | ---: |
| status | 2,442,048; 2,589,632; 2,278,272; 2,261,824; 2,442,176 | 2,442,048 | 2,938,637 → 2,938,637 (already passing) |
| run | 3,933,120; 3,933,184; 3,933,120; 3,998,784; 4,080,576 | 3,933,184 | 3,794,948 → **4,326,503** |
| Sixel TUI | 5,375,040; 6,194,176; 6,177,920; 5,440,576; 6,210,624 | 6,177,920 | 6,110,043 → **6,795,712** |

The new run and Sixel limits are exactly `ceil(median * 1.10)`, not maximum-derived limits or hand-picked round numbers. All recorded individual samples are strictly below their respective new limits. `.github/workflows/ship-gate.yml` and `test_ship_gate_calibrates_all_three_surfaces_at_n_five` change together. The advisory status and every-sample enforcement remain intact.

### Root cause and hold-out decision

The compared run snapshots show more allocator-retained freed pages with fewer live bytes. They do not establish the cause of every run-to-run allocation difference. The available v0.0.969 runner diagnostic (`969-footprint/run/run-1/vmmap-summary.txt`) has 132 KiB live allocated and 1,100 KiB fragmentation; the v0.0.970 first run has **121 KiB live** and **1,799 KiB fragmentation**, including 304 KiB in an empty medium region and 352 KiB in empty small regions. Allocator dirty pages rise 1,232 → 1,920 KiB even as live bytes fall. The observed allocator slack accounts for most of the footprint difference in these snapshots. However, the 969 harness terminal sequence is 23 versus 200 in 970: workload/harness evolution is a confound. This is not a controlled A/B or proof against every live-allocation regression, and it does not identify a single causal product commit. The brief's ~3.13 MB developer value is not a controlled same-host baseline; the retained 969 runner reading is 3,441,728 B.

The Sixel two modes have the same **1,206 KiB live allocated**. The low first sample has 1,914 KiB fragmentation / 3,120 KiB dirty allocation; high sample three has 2,682 KiB fragmentation / 3,888 KiB dirty allocation and an extra **736 KiB empty medium region**. High sample five similarly has 704 KiB in an empty medium region. Real image decode, resize/quantization scratch, and rendering run in every sample; freed medium-region scratch accounts for the roughly 0.8 MB mode separation. The cached live image/protocol is not the differential.

The carried-over candidate called `allocator_pressure_relief()` after headless dispatch and once after image encoding. It also added a wordmark test to ensure cached redraws did not trigger repeated relief. Those are plausible reclamation boundaries, but the applicable hold-out rule requires accepted footprint/CPU A/B evidence and the 1.06 MiB reply peak comparison. No accepted positive A/B was available, and freeing pages may incur later refault CPU costs. Therefore **all three candidate product edits were reverted**, including their test, rather than landing an unmeasured performance lever. No claim is made that retention was fixed in product code. The owner explicitly permits recalibration with a written cause; that is the selected solution.

### Local verification limit

`python3 scripts/perf/client-footprint-budget.py --self-test` passes (positive physical footprint and one thread). A direct `vmmap -summary` against a newly spawned local child exits **255**: `vmmap cannot examine process ... even though it appears to exist`, followed by `[fatal] mach port for process 0 not valid`. This is a real sandbox task-port restriction. No diagnostic override, sudo, alternate tool, or threshold change converts that failure into accepted local evidence. The actual release calibration attempts below fail the independent host-load acceptance guard before sampling. The requested local footprint acceptance remains blocked, and the final verdict is NO_SHIP.

`python3 -m unittest discover -s scripts/qa-gate/tests -p test_client_footprint_budget.py`: **11 passed**. This includes calibration count/settle/load requirements and the workflow budget pin. Existing runner evidence supports the recalibrated limits; local acceptance and a post-change CI job conclusion are not claimed.

Actual release CLI/TUI attempt (22 MiB `haider`, 24 MiB `haider-tui` produced by the ongoing scoped release build; CLI/TUI Rust sources match f724335d after hold-out reversion):

```sh
source tmp/shipgate/gate-env.sh
python3 scripts/perf/client-footprint-budget.py \
  --haider /private/tmp/haider-shipgate-target/release/haider \
  --surface tui-demo-sixel \
  --output tmp/shipgate/local-footprint/tui-sixel-fast-fail \
  --calibrate --runs 5 --budget-bytes 6795712 --load-wait-seconds 1
```

Exit **2**, `client-footprint: ENVIRONMENT-BLOCKED load_1m=5.15 limit=4.00` (`tmp/shipgate/local-footprint-sixel-fast-fail.log`). This shortens only the wait before reporting environmental failure; it does not relax load, settle, N=5, vmmap, or footprint acceptance. The preceding default-wait attempt was cancelled through its owned exec session (exit130) after continuing to wait for load below four during the full gate; no measurement was accepted. Direct byte inspection of the completed release daemon finds the new interactive/autonomous description and not the old blocking-question description. This confirms the model-prose update is present; it does not turn the blocked local attempts into accepted footprint evidence.


Release build completed in **29m26s** (`tmp/shipgate/build-release.log`); the sibling `haiderd` is **54 MiB**, above the required 10 MiB. The same strict command was then run for `run-post-command` (budget4,326,503) and `status-post-command` (budget2,938,637), each with `--calibrate --runs 5 --load-wait-seconds 1` and default 60-second settle / load limit4 / mandatory vmmap. Run exits **2**, `ENVIRONMENT-BLOCKED load_1m=7.37 limit=4.00`; status exits **2**, `ENVIRONMENT-BLOCKED load_1m=7.18 limit=4.00`. Logs are `tmp/shipgate/local-footprint-run-fast-fail.log` and `tmp/shipgate/local-footprint-status-fast-fail.log`. All three local surfaces were actually attempted; none reached an accepted sample. The load failures are distinct from the separately observed vmmap task-port restriction. No acceptance threshold was weakened and no extra release rebuild was performed.


## Gate evidence

CI-profile ladder completed: **14/16 PASS; 2/16 FAIL**. All 14 demo rungs pass. Both 118×36 and 90×10 live rungs pass automatic result, zero menu events/resolutions, live/cold reconstruction, no ERRORED, no credentials, and `/voice` flash. Both fail process enumeration (`pgrep` exit 3) and dependent singleton/R8 assertions. Authenticated cleanup returns 0 in both. Full logs: `tmp/shipgate/ladder-final.log`, `live-ci-118.log`, `live-ci-90.log`. No assertion was relaxed to classify this as 16/16.

The corrected CI build succeeded; `haiderd` is 75,847,104 bytes. Offline `haider self-test` returned `"ok":true`. The provider-request golden regenerated through `provider_request_body_is_budget_independent_and_matches_the_golden_ledger` and is unchanged. Scoped Clippy (`-p haider-daemon -p haider-cli -p haider-tui --all-targets --locked --keep-going -- -D warnings`) passed. The true-release render benchmark passes **2/2** tests: first-frame times at 10k/50k/200k rows are 345.958/420.625/320.250 microseconds, and the largest cached p95 is 190.333 microseconds. Full per-crate tests passed: **19/19 crates**, **5,633 passed / 0 failed / 13 existing ignored**. No ignore, skip, platform gate, or test-count waiver was added. `scripts/ci-test.sh` exits 0 with `fail=0`; per-crate logs and `full-test-summary.json` are retained under `tmp/shipgate/`. All builds use `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`; siblings are prebuilt before daemon tests. Cargo is wrapped with the required free-disk check (stop below 700 MiB). Logs and artifacts remain under `tmp/shipgate/`.

The regenerated request-input provider schema test passes. The actual native full-prefix pin is **21,541 → 21,637** (+96 UTF-8 description bytes); Linux/Windows/other platform pins retain their existing deltas (by inspection). The default invariant pipe is **6,166 → 6,166**, measured POSIX **6,244 → 6,244**; the independent half-size floor remains unchanged. Test-count tooling reports **5,151 → 5,151**.

Development-profile 90×10 smoke: all session/replay/menu/voice/credential/ERRORED checks pass, journal 71/71 decoded, result 1, menus/answers/resolutions 0. The two process-count/lifecycle checks fail because `pgrep` cannot access the process list (`sysmon request failed ... sysmond service not found`). This is not 16/16 evidence and the process assertions remain enforced.

Completed: Rust formatting; unsafe-count gate (production 189, test 21); release packaging tests (9); release evidence policy tests (53); QA harness tests (98); footprint harness tests (11). Python probe compilation and final whitespace checks also pass. A final fetch and merge-forward check again found `origin/wave-970` already at the same base SHA. No remote post-change CI/xplat success or non-macOS execution is claimed.

## CI error registry walk

- #19: formatting and whitespace checks; schema golden regenerated through repository tooling, not hand-edited.
- #20: test-count tooling recounts the merged tree; no test removal retained.
- #41: throwaway short profiles and existing environment scrub remain in the PTY probes.
- #44: live execution remains a mandatory gate; unavailable host capabilities do not become PASS.
- #64: check built daemon size exceeds 10 MiB before execution.
- #71: require actual CI-profile ladder and release-profile footprint evidence, not source inference.
- #80: preserve settled-versus-terminal distinction and strict calibration acceptance; advisory workflow state unchanged.
- #94: existing action waits retained; cleanup-only bound 22.5 seconds = CLI stop budget 20 seconds + existing probelib reap allowance 2.5 seconds. It is not an R8 witness.
- #95: no product waits or negotiated-connection logic changed.
- Other registry surfaces: no behavior change in this lane; cross-platform runtime behavior remains by inspection unless explicitly executed below.


## Independent verifier and verdict

The independent verifier found no qualifying code, test, or shipping-decision defect. Two documentation claims were corrected: the historical calibration is only pre-change evidence, and the run-footprint comparison has a workload confound. These accepted editorial corrections changed no code, test, or already-NO_SHIP verdict, so they are excluded from the owner's real/noise metric rather than counted as either real findings or rejected noise. The final probe isolation/cleanup logic, schema fixture, byte pins, and calibration arithmetic were independently reviewed without further findings.

Shipping remains blocked: R8/single-daemon process enumeration must pass on a capable host for 16/16, and all three strict release footprint surfaces need accepted local measurements. The revised limits accept every sample of the cited release-runner calibration; this is not a substitute for those missing local acceptances.

VERIFIER: findings=0 real=0 noise=0 — no qualifying findings
NO_SHIP
