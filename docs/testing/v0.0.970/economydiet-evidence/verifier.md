# Independent economydiet verifier

Overall acceptance: **SHIP**. Source, fixtures, merged workspace gate,
economy measurement, ABBA latency and the complete actual-binary T0 rerun
are approved. This review did not run builds, tests or benchmarks alongside
the root agent's validation.

Five findings were accepted and fixed; zero findings were rejected as noise.

| Finding | Fix and reviewed evidence |
| --- | --- |
| F1: Removing process/mutation receipts prevented model callers from supplying provenance required by verified graph slots. | Explicit `evidence_from` selectors resolve same-run terminal journal facts. Mixed selectors and explicit provenance are rejected; the existing store still validates authority, freshness and exit status. Parser, journal lookup and live verified-slot/replay pins cover the new path. |
| F2: The legacy `exec` alias retained the full model envelope. | Projection normalizes `exec` to `process_exec`, including bounded output and savings accounting. Alias first-send/replay pins retain the durable receipt. |
| F3: Standalone tool discovery lost its catalog or owned fallback during provider refresh. | The no-base path restores the full catalog before clearing its shared view; fallback capture handles owned and shared definitions. Two dedicated refresh/fallback regressions cover these paths. |
| F4: Manual removal also removed unique action constraints still enforced by tools. | Native schemas retain parameter semantics and validation bounds. Search context limits, edit-anchor uniqueness, path destinations and `.git` exclusion have visibility pins; actbias top-level descriptions remain verbatim. |
| F5: The new monitor QA signature used one space where the renderer emits two. | Signature and positive fixture now match the actual two-space text. Both retained frame sizes match; footer-only and unchanged-frame negatives remain. |

Reviewed discovery behavior includes deterministic catalog order, durable
settlement before promotion, session reconstruction, compaction rebinding,
provider refresh/fallback, and unchanged grant/lockdown filtering. The root
agent's separately discovered workspace-persistence correction preserves
discovered names while resetting permission and capability consent; it is not
counted as a verifier finding.

The merge from `origin/wave-970@7431f8e6` preserves the exhausted-cap check
before provider refresh. Compared with that upstream revision, the actor's
request-loop cap/ceiling code and request-budget law tests are unchanged.
Worker ceiling-workspace setup and restored request counts remain present.
Model-only envelope projection leaves durable process/filesystem producers,
effects and truncation provenance intact. The merged goldens were regenerated
through the recorded update flags and validated in the completed workspace
gate. Independent comparison of all 116 actual fixture records against
`7431f8e6` confirms only the documented provider policy/tool changes and the
`v4` to `v5` system-version substitution in durable JSONL records.

The ten entries in `merged-gate-steps.json` all exit zero. The workspace log
contains 336 successful result blocks totaling 5,387 passes, zero failures and
13 ignores; `merged-workspace-totals.json` separates these into 5,375 top-level
passes and 12 nested subprocess probes. Logs also confirm clippy with tests,
format checking, source-marker count 4,969, xtask checks, 66 Python QA tests
and four probe tests. The nine existing file-size soft-cap warnings are
reported accurately. Raw-evidence whitespace warnings are separately
classified; the source/authored-document check passes.

The table in `economydiet.md` agrees with `comparison-merged.json` and both
merged measurement files, including rounded percentages. Independent audit
checked every compared scalar and reduction, both raw-report SHA-256 hashes,
eight requests and 17 tool results per side, unchanged system/tool prefixes
within each capture, and all seven supplemental effect checks. Fixed overhead
is 14,222 to 6,031 AHRB reference tokens (57.59% lower); envelope overhead is
1,099.35 to 84.12 bytes/result (92.35% lower); reported wasted calls remain zero.
The clean baseline uses the same merged upstream revision. The report
correctly retains the official `terminal-without-effect` adapter failure on
both sides and labels the separate captured-call/filesystem evidence; it does
not substitute that supplemental check for official completion. The unusable
native-first supplementary capture is excluded from acceptance metrics.

ABBA timing is approved. Independent recomputation from all eight raw reports
matches every reported sample count, median and MAD; their SHA-256 hashes,
binary hashes and accepted/correctness statuses agree with `timing/abba.json`.
Each warm shape has 50 measured samples per side; one-shot has 42 per side.

| Shape | A median ± MAD, ms | B median ± MAD, ms | Result |
| --- | ---: | ---: | --- |
| Warm single request | 51.8556 ± 2.60125 | 47.2232 ± 2.17981 | Within criterion |
| Warm tool turn | 83.3680 ± 4.68813 | 74.8160 ± 3.50304 | Within criterion |
| One-shot | 107.4242 ± 2.08090 | 107.4343 ± 2.10481 | Within criterion |

All three satisfy B median ≤ A median + max(A MAD, B MAD). Recorded loads are
1.772–2.798, below the unchanged limit of 3. The raw reports identify the same
108 owned daemon PIDs as `timing/cleanup.json`, which records none alive and
clean warm-daemon stops. The earlier partial attempt failed at its first
one-shot warmup because a 133-byte IPC path exceeded macOS's 103-byte limit;
it contributes no samples. The final invocation uses `TMPDIR=/tmp` on both
sides without changing harness acceptance criteria. No accepted regression
was retried.

The final complete actual-binary T0 rerun passes all 14 checks, with zero
failures, skips or environment-blocked results. Independent inspection of
`qa-t0-retry/qa-gate-t0-Syeds-MacBook-Air.local-20260905T090540Z.json` confirms
all check statuses, `/update` with `exit_clean=true`, all 16 owned-daemon
cleanup rows passing with `alive_after=false`, and candidate binary hashes
identical to the accepted ABBA reports. Both gate execution and report-schema
validation exit zero. The earlier 13/14 result and subsequent isolated A/B
update probes remain retained; no assertion, deadline or production change
was made between the last failed full run and the successful full rerun.

The subsequent OSC probe-parser correction was independently reviewed with no
new finding. Its non-greedy OSC arm recognizes BEL and ST terminators. Applying
the corrected parser to all three retained raw attach captures preserves
`message haider` and removes the attach OSC payload. Regression pins cover both
terminators, multiple sequences and the original ST-before-later-BEL failure.

VERIFIER: findings=5 real=5 noise=0 — restored graph provenance selection; normalized exec projection; preserved standalone catalogs/fallbacks; retained action constraints; corrected monitor signature spacing

SHIP
