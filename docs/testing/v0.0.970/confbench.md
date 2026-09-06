# v0.0.970 conformance release gate

**Round 2 supersedes the round-1 verdict below.** The preserved 50% byte gate
is unchanged. The old adapter does **not** derive `failed` from `status`:
its failed-status projection and malformed predicate remain incompatible.
See the appended round-2 evidence for the compact declaration, fresh release
benchmark, merged full gate, and corrected 18-pass acceptance.

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

## Round 2: compact default delegation and repaired-attempt status

Acceptance is **18 PASS / 1 FAIL (timeout only) / 2 platform SKIP**:
21 − 2 − 1 = 18, matching the supplied 969 result. This supersedes the
round-1 discussion of a requested 19-pass count. The 50% economydiet gate
has not been relaxed or re-derived.

### Product findings and citation audit

The requested malformed-attempt status is already present at the starting
commit `72364f34`. `close_malformed_tool_failure` builds a failed result
(`actor.rs:8260`), `commit_tool_result_and_completion` maps the result status
to the item's status (`actor.rs:10074`), and `ToolResultStatus::item_status`
maps Failed to Failed (`tool.rs:370-376`). The existing round-1 JSONL completion
already carries `payload.item.status: "failed"`, plus the durable `failed`,
`reason`, and `repaired` metadata. No status rewrite is needed or claimed.

The assertion that the old adapter derives `failed: true` from `status: failed`
is **wrong for the actual peer files**. `adapter.toml:123` and
`normalize.py:149-165` remain correct citations for a projection that only
copies declared fields. `runner.py:928` still accepts only `failed is True`
or a harness error in the malformed case. Its ordinary failed-tool predicate
at line 915 additionally accepts failed status; the malformed predicate does
not. The sole adapter harness-error rule (`adapter.toml:151-155`) matches
`run_failed`; `runner.py:675-682` excludes parser diagnostics. Emitting a
run-level error would contradict the successfully repaired run's contract.
The supplied 969 malformed PASS exited **65** after one request; round 1
exited **0** after two. Its passing score therefore does not establish that
the old evaluator supports the new successful-repair semantics.

The [executed projection proof](confbench/round2/projection-proof.json),
[reproduction script](confbench/round2/projection-proof.py), and
[independent verifier record](confbench/round2/verifier.md) confirm this
against a real completion from the starting commit. The new CLI regression
executes a malformed `fs_read` and a valid corrected call with distinct IDs,
pins failed/completed statuses and repair metadata, requires exit 0 and no
`run_failed`, and compares the literal live event bytes with durable replay.
The core runtime pins also assert raw failed status before any repair request
and for exhausted repair allowance. The event-schema changelog and automation
contract state these semantics explicitly.

The auxiliary case originally failed because economydiet's eight-tool default
omitted `spawn_subagent`; neither a spawn stub nor spawn usage reached the
first request. Round 1 restored the complete definition. The peer's auxiliary
selector (`fake_proxy.py:296-321`, `:439-447`) selects the exact tool name
without needing the full schema. Its argument generator (`:507-546`) only
needs the required task/prompt properties for this call. Child dispatch parses
`SpawnSubagent::from_tool_args`; omitted selectors inherit the current
provider/model pair. No routing decision depends on the omitted optional
schema properties.

The proposed system-prompt-manual design is historical: merged economydiet
puts each tool's manual semantics once in its native description and has
**zero system manual bytes** (`worker.rs:2797-2800`, `:14567-14569`,
`:14694-14715`; the zero-manual test remains intact). Round 2 respects that
layout. Its default spawn view retains the exact required task/prompt
property schemas, adds compact usage and inherited-route guidance, and points
to `list_tools(filter="spawn_subagent")` for optional controls. The full
authorized catalog is unchanged. A committed discovery result supplies and
promotes the exact full definition; rejected discovery does not promote.
Configured/restored promotion, provider refresh and fallback retain the full
catalog and correct tool-pack digest. Tool/effect grants and lockdown still
bound execution. Unfamiliar schema shapes stay full rather than silently
losing constraints.

The supplied turnperf and turnperf2 evidence is contextual inspection evidence,
not an executed timing experiment here. D7-1/D7-7's call identity, continuation,
and existing atomic tool settlement remain unchanged; D7-X's fake-proxy proof
ledger is untouched. No durable boundary, retry budget or deadline changes.
Historical latency estimates are not reported as round-2 measurements.

### Before/after bytes

[Rust measurement](confbench/round2/gate/pipe-pin.log) and
[byte ledger](confbench/round2/bytes.json); all executed numbers are macOS arm64.
The pipe metric is name + native description + canonical schema + system
manual, excluding provider-dialect JSON framing. It is not a tokenizer result.

| Metric | Economydiet default without spawn | Round 1 full spawn | Round 2 compact spawn |
|---|---:|---:|---:|
| Advertised tools | 8 | 9 | 9 |
| Spawn name | 0 | 14 | 14 |
| Spawn native usage | 0 | 305 | 256 |
| Spawn schema | 0 | 2,000 | 304 |
| Spawn contribution | 0 | 2,319 | 574 |
| Default instruct pipe | 5,670 | 7,989 | **6,244** |
| System manual | 0 | 0 | 0 |
| Reduction vs 13,552 | 58.16% | 41.05% | **53.93%** |
| Preserved maximum | 6,776 | 6,776 | 6,776 |
| Headroom | 1,106 | −1,213 | **532** |
| Preserved 50% gate | PASS | FAIL | **PASS** |

The platform-invariant pin changes **7,911 → 6,166**; measured POSIX command
prose adds 78 bytes. The full authorized manifest remains **20,770** bytes,
registered count 30, policy 606 bytes, native description total 1,746 bytes.
Linux/Windows execution is not claimed: their unchanged platform-description
accounting plus the platform-independent 574-byte spawn contribution was
reviewed by inspection. No economydiet threshold is changed, and its prior
reference-token claims have not been remeasured or extrapolated.

The provider-request golden was regenerated through the repository's
`UPDATE_FIXTURES=1` test. [Structural comparison](confbench/round2/golden-diff.json)
shows only the spawn declaration changed from round 1; all other tool
schemas/descriptions and all other request fields are identical.

### Full gate on the merged round-2 tree

All commands used the [recorded ENV LAW](confbench/round2/execution-environment.json),
two build jobs, and four test threads. Every build-capable step recorded
`df -m /` before execution and respected the 700 MiB stop floor. Siblings
were prebuilt before setting `HAIDER_TEST_SIBLINGS_PREBUILT=1`; the debug
daemon is **201,747,808 bytes**, above 10 MiB. The release build and full
gate used separate targets and overlapped; no benchmark time is a performance
claim. [Exact gate commands and exits](confbench/round2/gate/steps.json) and
[full test totals](confbench/round2/gate/test-summary.json) are retained.

| Step | Exit | Result |
|---|---:|---|
| [siblings](confbench/round2/gate/siblings.log) | 0 | PASS |
| [protocol](confbench/round2/gate/protocol.log) | 0 | PASS |
| [malformed](confbench/round2/gate/malformed.log) | 0 | PASS |
| [core-exposure](confbench/round2/gate/core-exposure.log) | 0 | PASS |
| [pipe-pin](confbench/round2/gate/pipe-pin.log) | 0 | PASS |
| [jsonl-replay](confbench/round2/gate/jsonl-replay.log) | 0 | PASS |
| [golden](confbench/round2/gate/golden.log) | 0 | PASS |
| [baseline-before-gate](confbench/round2/gate/baseline-before-gate.log) | 0 | PASS |
| [workspace-test](confbench/round2/gate/workspace-test.log) | 0 | PASS |
| [clippy](confbench/round2/gate/clippy.log) | 0 | PASS |
| [test-count-update](confbench/round2/gate/test-count-update.log) | 0 | PASS |
| [test-count](confbench/round2/gate/test-count.log) | 0 | PASS |
| [fmt](confbench/round2/gate/fmt.log) | 0 | PASS |

`cargo test -q --workspace --no-fail-fast` exits **0**: **5,469 passed,
0 failed, 13 existing ignored**, across 340 emitted test-result groups
(including doc tests). No test was weakened, ignored or platform-gated.
`cargo clippy --workspace --tests -- -D warnings` exits **0**.
`xtask test-count --update` and the subsequent check exit **0**: the source
baseline changes **5,036 → 5,040** (+3 tool-exposure tests and +1 CLI
JSONL/replay test). Source test counts and emitted test-instance totals are
different measures. Formatting is green.

Executed named coverage includes `default_spawn_preserves_required_contract_and_full_discovery`,
`spawn_projection_survives_owned_refresh_and_provider_fallback`,
`default_spawn_projection_keeps_unfamiliar_schemas_intact`,
`malformed_attempt_stays_failed_after_successful_repair_in_jsonl_and_replay`,
`instruct_pipe_shrinks_the_advertised_wire_pack`, the existing malformed
runtime group, default delegation/grant/lockdown tests, actual child-spawn
integration, and the provider-request golden pin. Full gate is green on macOS;
Linux/Windows behavior remains by inspection only.

### Release benchmark: both case tables and raw repeats

The fresh native release pair was built with
`cargo build --release -p haider-cli -p haider-daemond`; exit **0**.
[Artifact hashes and sizes](confbench/round2/release/artifacts.json) identify
`haider` (**35,543,584 bytes**) and `haiderd` (**55,365,968 bytes**).
Both `--version` commands succeeded under a throwaway HOME/profile. They
report `0.0.969`, the unchanged package version on this wave-970 candidate;
the adapter's embedded v0.0.962 artifact identity is not the identity of the
overridden binaries. The pair's hashes are unchanged across all three runs.

The final [unchanged old-bench report](confbench/round2/haider-970-round2.json)
is **17 PASS / 2 FAIL / 2 SKIPPED**, exit **1**. Only malformed and timeout
fail. [Exact command](confbench/round2/benchmark-command.json), run from the
read-only peer `/Users/rizzist/Documents/CODING/haidercode-web`:

```sh
python3 -m bench.conformance --adapter haider-agent \
  --executable /Users/rizzist/haider-run/lane-970-confbench/target/release/haider \
  --model deepseek-v4-flash --context-window 131072 \
  --max-output-tokens 8192 --max-turns 20 \
  --process-timeout 15 --proxy-timeout 20 \
  --json-report /Users/rizzist/haider-run/lane-970-confbench/docs/testing/v0.0.970/confbench/round2/haider-970-round2.json
```

Table 1 retains the supplied baseline and pre-fix 970 results, with round 1
for comparison. These are prior evidence, not newly executed baselines.

| Case | 969 baseline | 970 before | Round 1 |
|---|---|---|---|
| exact_model_and_endpoint | PASS | PASS | PASS |
| allowed_request_paths | PASS | PASS | PASS |
| streamed_text | PASS | PASS | PASS |
| one_tool_call | PASS | PASS | PASS |
| multiple_tool_calls | PASS | PASS | PASS |
| parallel_tool_calls | PASS | PASS | PASS |
| fragmented_tool_call | PASS | PASS | PASS |
| failed_tool_call | PASS | PASS | PASS |
| malformed_tool_call | PASS | FAIL | FAIL |
| structured_terminal_success | PASS | PASS | PASS |
| structured_terminal_failure | PASS | PASS | PASS |
| retry_429 | PASS | PASS | PASS |
| retry_500 | PASS | PASS | PASS |
| stream_disconnect | PASS | PASS | PASS |
| timeout | FAIL | FAIL | FAIL |
| no_interactive_prompt | PASS | PASS | PASS |
| no_request_outside_allowlist | SKIPPED | SKIPPED | SKIPPED |
| no_persistent_state | SKIPPED | SKIPPED | SKIPPED |
| no_second_model_or_auxiliary_provider | PASS | FAIL | PASS |
| tool_call_id_dedup | PASS | PASS | PASS |
| patch_when_stdout_truncated | PASS | PASS | PASS |

Table 2 records **every** round-2 full run. The first run's extra failure was
`retry_429`: outside-state audit, exit 0, two requests. On the first unchanged
repeat the extra outside-state failure moved to `streamed_text`; `retry_429`
passed. The final unchanged repeat has no extra failure. This supports
transient outside-state interference, not a new retry or streaming defect.
All raw statuses remain intact: [first report](confbench/round2/haider-970-round2-audit.json),
[first repeat](confbench/round2/haider-970-round2-repeat-audit.json),
[final repeat](confbench/round2/haider-970-round2.json). No per-case results
are combined into an invented passing run.

| Case | First run | First repeat | Final repeat | Final requests |
|---|---|---|---|---:|
| exact_model_and_endpoint | PASS | PASS | PASS | 1 |
| allowed_request_paths | PASS | PASS | PASS | 1 |
| streamed_text | PASS | FAIL | PASS | 1 |
| one_tool_call | PASS | PASS | PASS | 2 |
| multiple_tool_calls | PASS | PASS | PASS | 3 |
| parallel_tool_calls | PASS | PASS | PASS | 2 |
| fragmented_tool_call | PASS | PASS | PASS | 2 |
| failed_tool_call | PASS | PASS | PASS | 2 |
| malformed_tool_call | FAIL | FAIL | FAIL | 2 |
| structured_terminal_success | PASS | PASS | PASS | 1 |
| structured_terminal_failure | PASS | PASS | PASS | 1 |
| retry_429 | FAIL | PASS | PASS | 2 |
| retry_500 | PASS | PASS | PASS | 2 |
| stream_disconnect | PASS | PASS | PASS | 1 |
| timeout | FAIL | FAIL | FAIL | 1 |
| no_interactive_prompt | PASS | PASS | PASS | 1 |
| no_request_outside_allowlist | SKIPPED | SKIPPED | SKIPPED | 1 |
| no_persistent_state | SKIPPED | SKIPPED | SKIPPED | 1 |
| no_second_model_or_auxiliary_provider | PASS | PASS | PASS | 3 |
| tool_call_id_dedup | PASS | PASS | PASS | 2 |
| patch_when_stdout_truncated | PASS | PASS | PASS | 2 |

The timeout remains the owner-identified host audit artefact; two procfs
network/state cases remain Darwin SKIPs. The owner's live daemon was neither
stopped nor modified. The old peer files are unchanged before/after all runs,
and match round 1's hashes ([peer ledger](confbench/round2/peer-file-sha256.json));
Python bytecode writes were disabled. The reports' durations and resource
fields are not credited as accepted performance measurements.

The release diagnostic captures exercise both regressions separately and retain
raw JSONL, normalized events, exits and request identities. They are additional
evidence, not replacements for the full benchmark:

| Regression | Executed release facts | Old bench result |
|---|---|---|
| malformed_tool_call | Failed result seq 26; exactly one completed call-1 at seq 27 with status failed, failed=true, reason and repaired=true; second request marker seq 39; one successful terminal seq 51; exit 0; two requests; no run_failed | FAIL: normalization retains failed status but omits the failed boolean |
| no_second_model_or_auxiliary_provider | spawn_subagent selected; actual agent_spawned seq 31; child_result seq 49; call-1 completed seq 51; successful terminal seq 71; exit 0; three requests, all deepseek-v4-flash at /v1/chat/completions | PASS |

See [asserted regression summary](confbench/round2/regression-summary.json),
[malformed selected records](confbench/round2/diagnostic/malformed_tool-selected.json),
and [auxiliary selected records](confbench/round2/diagnostic/auxiliary_probe-selected.json).
The CLI test additionally executes a genuinely corrected tool call and pins its
separate successful completion plus literal replay parity. Raw replay, runtime,
bench, native macOS build and gates were executed; alternate-platform behavior,
the absence of schema-based routing, and constraint/grant preservation were
also inspected. No Linux/Windows runtime or real-model-quality claim is made.

### Round-2 verdict, merge and delivery

**NO_SHIP** under the corrected acceptance. The full gate and unchanged 50%
size ceiling are green; default auxiliary capability is restored with a
574-byte declaration. The old bench still lacks its 969 passing case
`malformed_tool_call` ([passing-set comparison](confbench/round2/benchmark-summary.json)).
The product already emits the requested failed attempt status and successful
run terminal. An old-adapter projection or predicate correction is still
needed to recognize that truthful result; emitting run_failed would violate
the requested behavior. This lane does not change the peer or relabel a FAIL.

Ordinary Git fetch/merge/staging cannot write this worktree's external,
read-only metadata. The authorized writable mirror `/tmp/confbench-lane.git`
retains branch `lane-970-confbench`. It fetched and merged forward before the
full gate, then fetched/merged again after execution: upstream remains
`620fc1ce2faf0344c2d27c172fabe2c2f99bd253`, already contained in starting commit
`72364f34`. Both mirror merges returned `Already up to date`;
[merge evidence](confbench/round2/merge.json) is retained. Thus the final gate
and release artifacts include the current merged upstream. The provider
request golden was regenerated through tooling, the byte pin measured, and
the test baseline recounted. No golden was hand-merged.

The final lane commit is retained in the writable mirror, with its exact ID
reported at delivery and a portable bundle at
`tmp/confbench/lane-970-confbench-round2.bundle`. No push or trailer is added.
The supplied `LANE-COMMON.md`, `LANE-BRIEF-confbench.md`, `turnperf/`, and
`turnperf2/` inputs remain untracked and excluded. Only closed text logs have
trailing whitespace and redundant final blank lines removed for whitespace checking; raw benchmark
JSON and JSONL are unchanged, with formatting hashes recorded separately.

The independent verifier found **no new findings** in two reviews. It confirmed
the known malformed predicate blocker and independently verified the compact
spawn route, full discovery retention, and byte arithmetic. These confirmations
are not counted as novel findings; benchmark audit interference is separately
reported above. See the [verifier record](confbench/round2/verifier.md).

VERIFIER: findings=0 real=0 noise=0 — no new findings; existing malformed adapter blocker confirmed
NO_SHIP
