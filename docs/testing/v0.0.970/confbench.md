# v0.0.970 conformance release gate

The old read-only peer benchmark has two incompatibilities with the requested
release acceptance: its malformed-call evaluator ignores the failed status its
own adapter emits, and 21 cases with two Darwin skips and the excluded timeout
failure can produce at most 18 passes. The unchanged benchmark result must be
reported without reclassifying either failure or the skips as passes.

## Scope and root causes

The supplied lane files and turnperf/turnperf2 evidence were read before changes.
Their durable journal, call identity, raw replay and fixed-route requirements
remain applicable. No timing optimization or durability boundary moves here.
Round-2 D7-1/D7-X specifically preserve tool identity/continuation and the fake
proxy's proof ledger. Historical timing estimates are not new measurements.

`malformed_tool_call` was already visible in the 970 raw stream: failed result
and completion, exit 0, two requests, with the failed completion before the
repair. The loss occurs in benchmark interpretation, not in JSONL delivery.
`bench/adapters/haider-agent/adapter.toml:123` extracts only call ID, tool name
and status. `bench/adapters/normalize.py:149-165` builds output only from the
adapter's declared fields/constants. `bench/conformance/runner.py:928` requires
`failed is True`; its adjacent ordinary failed-tool test at line 915 also
accepts `status: failed`. Added product fields cannot cross that projection.
A recoverable `run_failed` would be false terminal evidence and violate the
run contract. No peer file or adapter is changed.

The product now adds durable completion payload fields `failed: true`,
`reason: malformed_tool_call`, and `repaired: true|false`; the typed invalid-call
result also retains the optional repair flag. `true` records the available
non-terminal repair allowance, not eventual repair success. Exhaustion records
`false`. Both records retain IDs, status and raw arguments and commit atomically
before any repair request. Raw replay preserves the exact metadata.

The auxiliary failure occurs before child selection: the 970 report records
`auxiliary_tool_matched: false`. The first provider request omitted
`spawn_subagent`, so fake_proxy.py:720-731 fell back to an ordinary tool and
never launched an auxiliary request. Restoring `spawn_subagent` in the default
exposure fixes that trigger. Exposure still intersects the authorized catalog;
tool/effect grants, provider refresh and lockdown continue filtering it.
The bench supplies only required task/prompt fields; omitted child model/provider
use the inherited pair. Exact-selector and credential policies need no change.

Citation audit: runner.py ~900-950 correctly locates the malformed predicate
(now 927-931), but the auxiliary predicate drifted to 972-992. The hypothesized
resolve_child_selector mechanism exists, but is not reached by the missing-tool
probe. D7 actor message movement citations 3411/4041 drifted to 4031/4707;
worker catalog 12308 drifted to 13758. No old timing estimate is relied on.

## Supplied baseline reports

These reports are copied unchanged from the owner's tmp/bench970 evidence.
The adapter's pinned artifact/version identity describes its v0.0.962 manifest,
not the overridden executable. Revision identity and actual executable hashes
are reported separately for this lane's run.

- [969 report](confbench/haider-969.json): 18 PASS / 1 FAIL / 2 SKIPPED.
- [970 before report](confbench/haider-970-before.json), supplied revision
  471b9d68: 16 PASS / 3 FAIL / 2 SKIPPED.

| Case | 969 | Requests | 970 before | Requests |
|---|---|---:|---|---:|
| exact_model_and_endpoint | PASS | 1 | PASS | 1 |
| allowed_request_paths | PASS | 1 | PASS | 1 |
| streamed_text | PASS | 1 | PASS | 1 |
| one_tool_call | PASS | 2 | PASS | 2 |
| multiple_tool_calls | PASS | 3 | PASS | 3 |
| parallel_tool_calls | PASS | 2 | PASS | 2 |
| fragmented_tool_call | PASS | 2 | PASS | 2 |
| failed_tool_call | PASS | 2 | PASS | 2 |
| malformed_tool_call | PASS | 1 | FAIL | 2 |
| structured_terminal_success | PASS | 1 | PASS | 1 |
| structured_terminal_failure | PASS | 1 | PASS | 1 |
| retry_429 | PASS | 2 | PASS | 2 |
| retry_500 | PASS | 2 | PASS | 2 |
| stream_disconnect | PASS | 1 | PASS | 1 |
| timeout | FAIL | 1 | FAIL | 1 |
| no_interactive_prompt | PASS | 1 | PASS | 1 |
| no_request_outside_allowlist | SKIPPED | 1 | SKIPPED | 1 |
| no_persistent_state | SKIPPED | 1 | SKIPPED | 1 |
| no_second_model_or_auxiliary_provider | PASS | 3 | FAIL | 2 |
| tool_call_id_dedup | PASS | 2 | PASS | 2 |
| patch_when_stdout_truncated | PASS | 2 | PASS | 2 |

The timeout failure is the owner-identified host artefact: the real
`~/.haider` audit sees writes from the owner's live daemon during the nine-second
case. Neither that daemon nor the benchmark audit was changed. Network and
persistent-state procfs audits remain the benchmark's two Darwin skips.

## Final merged release benchmark

[Unchanged final report](confbench/haider-970-after.json): **17 PASS /
2 FAIL / 2 SKIPPED**. The executable pair was built with
`cargo build --release -p haider-cli -p haider-daemond` from the merged tree.
Both siblings' `--version` commands were run with a temporary HOME/profile before
the benchmark. [Release hashes and sizes](confbench/release-artifacts.json)
identify the tested artifacts. The package still reports `0.0.969`; this is the
wave-970 candidate, not a version-bumped release. The peer adapter's embedded
v0.0.962 identity is not used to identify these binaries.

The exact command ran from the read-only peer directory:

```sh
python3 -m bench.conformance --adapter haider-agent \
  --executable /Users/rizzist/haider-run/lane-970-confbench/target/release/haider \
  --model deepseek-v4-flash --context-window 131072 \
  --max-output-tokens 8192 --max-turns 20 \
  --process-timeout 15 --proxy-timeout 20 \
  --json-report /Users/rizzist/haider-run/lane-970-confbench/docs/testing/v0.0.970/confbench/haider-970-after.json
```

| Case | 969 | Merged 970 | Merged requests |
|---|---|---|---:|
| exact_model_and_endpoint | PASS | PASS | 1 |
| allowed_request_paths | PASS | PASS | 1 |
| streamed_text | PASS | PASS | 1 |
| one_tool_call | PASS | PASS | 2 |
| multiple_tool_calls | PASS | PASS | 3 |
| parallel_tool_calls | PASS | PASS | 2 |
| fragmented_tool_call | PASS | PASS | 2 |
| failed_tool_call | PASS | PASS | 2 |
| malformed_tool_call | PASS | FAIL | 2 |
| structured_terminal_success | PASS | PASS | 1 |
| structured_terminal_failure | PASS | PASS | 1 |
| retry_429 | PASS | PASS | 2 |
| retry_500 | PASS | PASS | 2 |
| stream_disconnect | PASS | PASS | 1 |
| timeout | FAIL | FAIL | 1 |
| no_interactive_prompt | PASS | PASS | 1 |
| no_request_outside_allowlist | SKIPPED | SKIPPED | 1 |
| no_persistent_state | SKIPPED | SKIPPED | 1 |
| no_second_model_or_auxiliary_provider | PASS | PASS | 3 |
| tool_call_id_dedup | PASS | PASS | 2 |
| patch_when_stdout_truncated | PASS | PASS | 2 |

Literal baseline passing-set inclusion is **false**. The missing set is
`malformed_tool_call`. The auxiliary case is restored;
the malformed predicate still discards the product's explicit structured
failure through its unchanged adapter projection. This is a reported benchmark
bug, not a product terminal error added to satisfy the evaluator. No raw FAIL
or SKIPPED result is credited as PASS. The requested 19-pass count remains
incompatible with the specified two skips and timeout failure.

The [peer file hashes](confbench/peer-file-sha256.json) are unchanged before and
after execution. `PYTHONDONTWRITEBYTECODE=1` prevented Python cache writes in the
peer. Separate fake-proxy diagnostic captures retain raw JSONL, normalized
output, exit status and every observed model/path in
[diagnostic/](confbench/diagnostic/). Those captures are diagnostic evidence,
not replacements for the full 21-case report.

The final raw malformed capture has the invalid result at sequence 26, exactly
one completed `call-1` at 27 carrying all three additive failure fields,
the next provider-attempt start at 38, and successful terminal at 51. The
[selected raw records](confbench/diagnostic/malformed-selected-events.json)
retain those facts. The unchanged normalizer produces `tool_finished` with
`status: failed` but drops `failed`, `reason`, and `repaired`, demonstrating the
projection bug directly. The auxiliary capture has exit 0, three requests all
using `/v1/chat/completions` and `deepseek-v4-flash`, and proxy/start/finish ID
sets exactly `{call-1}`; its normalized terminal is successful.

## Verification history and preserved size gate

The final merged results are recorded above and below. The first
pre-merge release benchmark produced 16 PASS / 3 FAIL / 2 SKIPPED: malformed,
timeout, and an additional `tool_call_id_dedup` audit failure. That last case
exited 0 with matching model and request paths, but its outside-state audit
counted 17 changed paths before the report emitted its ID check. The repeat
explicitly confirms matching IDs. An unchanged-binary repeat produced 17 PASS /
2 FAIL / 2 SKIPPED, with only malformed and timeout failing. Both original
reports are retained; no status was relabeled:

- [First pre-merge run](confbench/haider-970-premerge-audit-noise.json).
- [Unchanged pre-merge repeat](confbench/haider-970-premerge.json).
- [Pre-merge executable identities](confbench/premerge-release-artifacts.json).

The [initial pre-merge workspace log](confbench/premerge-gate/workspace-test.log)
records three failures: the preserved size ceiling, the old eight-tool golden
expectation, and an incorrect new test assumption that standard lockdown excludes
spawn. The latter two were corrected: the golden pins the intended nine-tool
surface; standard lockdown already admits spawn, while an explicitly filtered
allowed-tool list still excludes it. No lockdown product policy was changed.
The pending pre-merge retry was interrupted for the upstream merge; the merged
full gate below is the final gate. The [initial gate steps](confbench/premerge-gate/steps.json)
and logs are retained for that distinction.

An earlier first launch of the newly built debug executable timed out after
15 seconds with zero proxy requests and empty output. An unchanged retry
completed the malformed scenario with two requests, the durable failed attempt,
and successful repair. This launch anomaly is recorded, not treated as proof
of a particular OS cause.

The independent verifier found one additional release invariant: the restored
full default spawn declaration adds 2,319 bytes (14 name + 305 native prose +
2,000 schema). Rust measured the default instruct pipe at 5,670 → 7,989 bytes,
while the unchanged 50% reduction gate allows at most 13,552 / 2 = 6,776.
The exact pin is updated to the intended surface; the reduction assertion is
preserved. Removing all schema descriptions would still leave only 155 prose
bytes under that ceiling, insufficient for the current child semantics.
No constraints, optional properties, grants, descriptions or existing default
tools were removed to manufacture a passing size result. The focused Rust test confirms registered=30, advertised=9, full prefix=20,770,
policy=606, instruct pipe=7,989 and native descriptions=1,795 bytes. Its exact
pin passes; only the preserved `pipe_bytes * 2 <= 13_552` assertion fails.
The reduction is 41.05%, below the required 50%. The merged upstream pin
accounts for platform-specific process-command description serialization;
its platform-invariant component changes 5,592 → 7,911. The actual macOS total
remains 5,670 → 7,989. Linux/Windows behavior is reviewed by inspection;
execution on those platforms is not claimed.

Verifier tracking: findings=1, real=1, noise=0. The prompt-size invariant changed
the implementation verdict from provisional SHIP by inspection to NO_SHIP.
The two already-diagnosed benchmark acceptance issues are research findings,
not additional pre-finish verifier findings.

## Full gate on the merged tree

Every build-capable command used the ENV LAW:

```sh
export RUST_MIN_STACK=8388608
export HAIDER_DISCOVERY_DISABLED=1
export HAIDER_TEST_DEVICE_NAME=test-mac
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
```

The debug gate additionally used `CARGO_TARGET_DIR=/tmp/confbench-debug`,
`CARGO_BUILD_JOBS=2`, and `RUST_TEST_THREADS=4`. Both siblings were prebuilt
successfully before `HAIDER_TEST_SIBLINGS_PREBUILT=1`; the debug daemon is
201745776 bytes, above 10 MiB. `df -m /`
was recorded before each build-capable step and stayed above the 700 MiB floor.
The [gate ledger](confbench/gate/steps.json) retains exact arguments and exit
codes; elapsed build/test times are operational records, not performance claims.
Four short text logs omit a redundant blank line at EOF to satisfy the repository
whitespace check; [formatting hashes](confbench/log-formatting.json) record that
format-only change. JSON benchmark reports and raw JSONL are unchanged.

| Step | Exit | Result |
|---|---:|---|
| [siblings](confbench/gate/siblings.log) | 0 | PASS |
| [protocol](confbench/gate/protocol.log) | 0 | PASS |
| [malformed](confbench/gate/malformed.log) | 0 | PASS |
| [core-exposure](confbench/gate/core-exposure.log) | 0 | PASS |
| [pipe-pin](confbench/gate/pipe-pin.log) | 101 | FAIL: preserved size ceiling |
| [golden](confbench/gate/golden.log) | 0 | PASS |
| [baseline-before-gate](confbench/gate/baseline-before-gate.log) | 0 | PASS |
| [workspace-test](confbench/gate/workspace-test.log) | 101 | FAIL: preserved size ceiling |
| [clippy](confbench/gate/clippy.log) | 0 | PASS |
| [test-count-update](confbench/gate/test-count-update.log) | 0 | PASS |
| [test-count](confbench/gate/test-count.log) | 0 | PASS |
| [fmt](confbench/gate/fmt.log) | 0 | PASS |

The mandatory `cargo test -q --workspace --no-fail-fast` exits **101** with
exactly one failed test:
`permissions_core_tests::instruct_pipe_shrinks_the_advertised_wire_pack`.
The exact byte assertion passes; its independent 50% reduction assertion fails.
The full run records 5,465 passed, 1 failed and 13 existing ignored tests across
341 groups (including doc tests, distinct from the source count). All other
executed tests pass. Existing ignored tests are unchanged; no test
was ignored or platform-gated to reach green.
`cargo clippy --workspace --tests -- -D warnings` exits **0**.
`cargo run -q -p xtask -- test-count --update` and the subsequent count check
exit **0**, baseline **5,036**. Formatting and whitespace checks pass.

Named behavior coverage includes:

- `invalid_tool_call` protocol goldens: old omitted field and new true/false
  serialization; the malformed runtime group runs seven tests for durable
  ordering, a single completion, replay, exhaustion and recovery.
- `agent_spawn_without_tool_exposure_completes_the_actual_child` and
  `production_spawn_effect_wait_and_report_chain_is_end_to_end`: default
  delegation reaches an actual child and retains its provider/model pair.
- `default_delegation_does_not_restore_a_tool_removed_by_provider_refresh` and
  `default_delegation_exposure_preserves_tool_and_effect_grant_ceilings`:
  discovery cannot bypass authorized tool/effect filtering; standard and
  explicitly narrowed lockdown packs retain their policy.
- `provider_request_body_is_budget_independent_and_matches_the_golden_ledger`:
  the regenerated merged request surface is deterministic.

The release verdict is **NO_SHIP**: the unchanged benchmark acceptance is not
met, and restoring the required default delegation violates an independent
preserved release budget. No bounded semantics-preserving size correction was
established within this lane. The findings are retained for the release owner;
neither the peer adapter nor a release threshold was weakened. The benchmark
owner can correct the mismatch by projecting the additive fields or accepting
`status: failed` in the malformed predicate, and reconcile the requested pass
count with Darwin skips. The product still needs a semantics-preserving way to
expose default delegation within the existing prompt-size budget.

## Merge and commit environment

The worktree's Git metadata is read-only. The requested ordinary fetch failed
on FETCH_HEAD and merge failed on ORIG_HEAD. A separate writable Git directory
at `/tmp/confbench-lane.git` preserves the same lane branch and worktree. Its
network fetch initially confirmed `6c6164c93644ffcd9ef3c8c3c65fc34432d7fb0a`.
Upstream advanced during verification, so the pending pre-merge final test run
was interrupted and `git merge --no-commit origin/wave-970` fast-forwarded to
`620fc1ce2faf0344c2d27c172fabe2c2f99bd253` before the final gate. This includes
the release-gate and cross-platform corrections.

Reapplying this lane resolved the three expected overlaps: the instruct-pipe
pin retains upstream's dynamic platform accounting and this lane's measured
spawn contribution; the provider-request golden is regenerated with
`UPDATE_FIXTURES=1` through the repository test; the test baseline is recounted
with `xtask test-count --update`. The golden is never hand-merged. Upstream's
Windows fixture normalization and lockdown policies remain intact.
Structural comparison against the merged upstream golden confirms eight →
nine tools, only `spawn_subagent` added, all existing tool declarations equal,
and every other provider-request field unchanged. The authoritative merged
source test count is 5,033 → 5,036 (+3 tests); the full gate uses the updated
baseline.

The actual worktree branch ref cannot be updated in this sandbox. The lane
commit is retained on `lane-970-confbench` in that writable Git
repository, with a portable bundle at `tmp/confbench/lane-970-confbench.bundle`
under this worktree. The bundle contains the merged upstream history after
`6c6164c9` plus this lane's commit; its exact ID is in the delivery message.
Nothing is pushed. The supplied lane notes and turnperf/
turnperf2/ evidence remain untracked and are excluded from that commit.
