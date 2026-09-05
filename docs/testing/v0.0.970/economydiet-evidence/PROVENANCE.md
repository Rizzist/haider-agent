The initial tree was `368f093c6aab5c90b408d9f9206071eb74234c77`. The baseline was built and the primary eight-request AHRB capture completed before product edits. Frozen binary hashes and sizes are in `baseline-binaries.json`; candidate hashes are in `candidate-binaries.json`. Both use the same unoptimized development profile, with debug information and incremental compilation disabled.

Before final verification, upstream advanced to `7431f8e6e9500729362cc4eb3cfb2bbc62cf462a` (`ceilingdecl`). A fresh clean checkout at `/private/tmp/economydiet-upstream-baseline` was built with the same command/profile and frozen to `.economydiet-upstream-baseline`. Its hashes are in `upstream-baseline-binaries.json`; its capture and analysis are `before-merged-ahrb/` and `before-merged-measurement.json`. Every scalar economy measurement is identical to the original claim-audit baseline. This separate build permits final latency comparison against the exact merge-forward base while preserving the original audit.

The final merged candidate is frozen separately in `.economydiet-merged-candidate`; its hashes are in `merged-candidate-binaries.json`. Final acceptance measurements use `before-merged-measurement.json`, `after-merged-measurement.json`, and `comparison-merged.json`. Its fixed prefix and envelope-overhead values match the earlier candidate exactly; the temporary benchmark profile path was one character shorter, reducing six write-result output strings by one byte each and slightly changing total content bytes and the context slope. The same scripted task, tokenizer, eight-request budget, and adapter are used on both sides.

Build command (after checking `df -m /` exceeded 700 MiB):

```sh
RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_TARGET_DIR=/private/tmp/haider-economydiet-target cargo build -p haider-cli -p haider-daemond --bins -j 2
```

Primary capture command, run once with each frozen binary directory first on `PATH`:

```sh
PATH="$PWD/.economydiet-baseline:$PATH" RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac /Users/rizzist/Documents/CODING/harness-bench/target/debug/ahrb run --pillar economy --manifest /Users/rizzist/Documents/CODING/harness-bench/adapters/haider-agent/manifest.toml --profile quick --output "$PWD/docs/testing/v0.0.970/economydiet-evidence/before-ahrb" --no-save
python3 scripts/qa-gate/economydiet_measure.py --report docs/testing/v0.0.970/economydiet-evidence/before-ahrb/report.json --bench-root /Users/rizzist/Documents/CODING/harness-bench --output docs/testing/v0.0.970/economydiet-evidence/before-measurement.json
```

For the candidate, substitute `.economydiet-candidate`, `after-ahrb`, and `after-measurement.json`. The benchmark repository is read-only: its existing executables, adapter, specification, source, and reference vocabulary are read; `--no-save` directs report writes exclusively to this lane and temporary isolated profiles. The adapter SHA-256 is `38f9a9c622ae69523231e0dfd99a7f97d9c6a0b8c350e6f14d21425e6504554d`.

The final merged comparison uses the same commands with these exact substitutions:

| Side | Frozen binary directory | AHRB output directory | Analyzer output |
| --- | --- | --- | --- |
| A | `.economydiet-upstream-baseline` | `before-merged-ahrb` | `before-merged-measurement.json` |
| B | `.economydiet-merged-candidate` | `after-merged-ahrb` | `after-merged-measurement.json` |

All report directories and analyzer outputs above are under this evidence directory. The frozen binaries are build inputs retained locally, excluded from the commit. Their sizes and SHA-256 hashes are recorded in the corresponding binary manifests.

The independent analyzer implements AHRB's exact common-block prefix separately for leading system/developer messages and tool definitions. It reproduces the reference BPE using AHRB's pinned vocabulary and rejects a measurement unless every primary canonical-request count, the combined prefix count, and the Theil–Sen context slope agree with the authoritative report. Reference tokens are provider-neutral AHRB units, not OpenAI tokenizer counts. System/tool side envelopes each include their own JSON framing; the combined stable prefix frames both once.

Tool-result content bytes count the first model-facing text for each unique tool-call ID. Envelope overhead subtracts decoded semantic output, preserving the same accounting for a JSON receipt plus truncation line before the change and plain output plus truncation line afterward. The retained `[haider:truncated …]` line counts as envelope overhead in both versions. The analyzer recognizes the filesystem mutation receipt's `result` string and the process receipt's `output` string; unrelated raw JSON output is not treated as a receipt.

The bundled adapter reports `terminal-without-effect` on both primary captures. Its rule observes the durable `started` tool-call item, whose arguments are still empty; AHRB schema 4 requires the normalized result to carry the final `arguments.path`. The official completion and waste metrics are not rewritten. Supplemental `independent_effect_evidence` joins captured provider-call IDs (and the actual `process_exec` argv), first model readback, and AHRB's external before/after filesystem and workspace receipts. All seven checks pass before and after, proving the scripted file was created with the required digest and read back. This does not claim real-model task success.

`native-priority-adapter.toml` is a generated copy of that adapter with only write/read alias order changed to prefer `fs_write`/`fs_read`. Its SHA-256 is `b52efe3a9767a2d18a1133c059eda4189c1e6425b218d66f883a8de255080145`. The supplementary baseline completes eight requests and its independent effect checks pass. The candidate supplementary run is unusable for a before/after comparison: preserving full native schema constraints removes the benchmark's undeclared `route` argument. After the five successful bootstrap writes, AHRB misclassifies the next request as another bootstrap and rejects it. The unmodified primary adapter retains routing metadata inside the valid shell command string and completes all eight requests. No product schema was weakened to accommodate the supplementary adapter.

Timing uses `economydiet_abba.py` with frozen A/B binaries and the existing turn-wall harness, preserving its sample counts, load limit, journal checks, daemon identity, and exact cleanup. Each complete comparison runs A-B-B-A suites and requires candidate median ≤ baseline median + max(baseline MAD, candidate MAD). A rejected high-load suite is retained as refusal evidence and cannot support a timing verdict.

The final accepted timing command was:

```sh
TMPDIR=/tmp RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac python3 -u scripts/qa-gate/economydiet_abba.py --baseline .economydiet-upstream-baseline --candidate .economydiet-merged-candidate --output-dir docs/testing/v0.0.970/economydiet-evidence/timing
```

`load-wait.json` records the cooldown before launch. Earlier attempts are preserved separately: `timing-refused-load/` exceeded the existing load limit; `timing-refused-runtime-path/` accepted its first warm suite but could not start its first one-shot warmup because macOS's inherited long temporary directory produced a 133-byte IPC endpoint above the 103-byte limit. The final invocation uses `/tmp` for both sides, matching the warm harness's existing short-root convention. Neither incomplete attempt contributes samples to the final comparison, and no accepted regression was retried.

All eight final suites passed their original proof pins. Each side contributes 50 warm samples per shape and 42 one-shot samples, excluding the existing warmups. The observed load range was 1.77246–2.79785, always below 3. The final wall medians/MADs are: warm single 51.855625 ± 2.6012495 → 47.2232085 ± 2.1798125 ms; warm tool 83.3680005 ± 4.6881255 → 74.8159995 ± 3.503041 ms; one-shot 107.424208 ± 2.080896 → 107.4343335 ± 2.1048125 ms. All three comparisons satisfy the non-regression criterion. `timing/cleanup.json` additionally confirms all four warm daemons stopped cleanly and all 108 known owned daemon PIDs from warm and one-shot samples/warmups were gone before the task was handed back. Only those known task-owned PIDs were checked.
