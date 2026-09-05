# v0.0.970 toolrepair

## Claim audit (before implementation)

Base branch: `lane-970-toolrepair`, base commit `e1aca96c`. The supplied benchmark report says 3/20 runs
failed on malformed tool JSON, but does not include their raw call payloads or
journals. This is supplied evidence, not a benchmark rerun or proof that these
changes improve the success rate.

| Claim / original citation | Audit on this lane before edits |
| --- | --- |
| Malformed JSON terminalizes the tool and run (`actor.rs:5132–5160,7300–7304`) | Correct behavior, drifted lines: `ToolCallEnd` catches `malformed-tool-arguments` at 5230–5268, durably closes the call, then returns provider failure. The helper is at 7394–7430; parsing is at 11029–11055. |
| Nested `fs_edit` schema and fresh-read/exact-anchor requirements | Correct: `filesystem.rs:480–528` schemas; pre-edit Unix implementation at 6513–6550 requires read/digest freshness and exact anchor counts. The old schema citation starts at 481 (drifted by one). Current descriptions are already preserved by the actbias changes, unlike the older investigation's native-description claim. |
| OpenCode lowercase repair and invalid result (`llm.ts:283–298`), flat edit and nine replacers | Correct, checked directly against official v1.18.9 source: [llm.ts:283–298](https://github.com/anomalyco/opencode/blob/v1.18.9/packages/opencode/src/session/llm.ts#L283-L298) lowercases a matching name and otherwise synthesizes an invalid call; [edit.ts:43–52,642–653](https://github.com/anomalyco/opencode/blob/v1.18.9/packages/opencode/src/tool/edit.ts#L642-L653) has flat fields and nine replacers. The investigation's installed executable was v1.17.20, so benchmark binary identity and causal impact remain unverified. |
| turnperf evidence supports changing durable boundaries | No. Round 2 documents failed boundary fusion and duplicate-request regressions. This lane retains result-before-continuation ordering and the existing provider-attempt boundary. No latency claim is made. |

The explicit user instruction to commit overrides LANE-COMMON's older instruction
to leave changes uncommitted. Supplied lane briefs and turnperf evidence are excluded.

## Implementation and verification

The actor closes malformed JSON and non-object arguments with failed status,
`ToolResultData::InvalidToolCall { tool, message }`, a parser diagnostic and repair
instructions. The failed result/item pair commits before another provider attempt.
Raw arguments remain journaled; live and replayed provider histories use the same
empty-object placeholder. A second consecutive malformed call persists its failure
and then terminates. Request, cost, cancellation and deadline ceilings still apply.

A valid frame following an invalid one persists a prompt-omitted reset before
execution. This rare-path marker makes restart behavior independent of response
epochs and deferred-result completion order. On checkpoint recovery, paged journal
reads recover the allowance and original name spelling. Read failure leaves the
checkpoint recoverable; reset-write failure closes open items before a terminal.

Name matching first accepts exact names, then accepts only one advertised name
after ASCII case folding and underscore removal. Ambiguous/unadvertised names are
not repaired. The result preview reports `{requested, resolved}` without changing
its status, images, artifacts or existing structured facts. Existing grant ceilings
and tool preflight still see the resolved canonical declaration.

`edit(file_path, old_string, new_string, replace_all?)` and
`write(file_path, content)` have strict flat schemas and decode directly to the
existing `FsEdit`/`FsWrite` operations. Both use the same registry effects/defaults,
broker lock, permission checks, freshness ledger, checkpoint/CAS handling and atomic
mutation implementation. Existing files still need a fresh read; edit anchors must
match exact bytes uniquely unless `replace_all` is explicitly true. Existing reduced
lockdown packs retain their original tool policy.

Anchor misses explain exact-byte/whitespace or multiplicity failure and show a
nearby line window. Ranking uses whitespace-normalized character bigram similarity
only for diagnostics. It never applies a fuzzy replacement. The failure-only scan
retains at most 16 clipped lines and 512 characters per candidate, redacts credentials
before clipping, preserves PEM redaction state, and suppresses sensitive paths.
An empty file explicitly reports that no candidate exists. Windows and Unix call
the same diagnostic helper; Windows/Linux execution remains **by inspection** here.

The canonical registry changes are narrowly in worker registration, definitions,
manual lines, grant lists and argument decoding. No worker lifecycle, OAuth,
provider transport, request cap or compaction logic changed. Registry consumers and
serialized-size pins are updated additively; dependencies and lockfile are unchanged.

The initial workspace run exposed one CLI request golden that needed the new
schemas. A semantic comparison proved that removing only the `write`/`edit` tool
definitions and their two manual lines exactly restores the old request. The
fixture was regenerated with `UPDATE_FIXTURES=1`; the test's budget-independent
and warm/cold request equality assertions remain intact. Text/tool JSONL and replay
goldens required no changes.

## Named regression evidence

| Contract | Tests |
| --- | --- |
| One durable invalid result and repair; second terminates | `malformed_tool_json_is_durable_invalid_result_with_one_repair_continuation`, `second_consecutive_malformed_tool_json_terminates_after_one_repair`, `two_malformed_calls_in_one_response_terminate_without_a_repair_send` |
| Valid reset and non-object arguments | `valid_tool_call_resets_malformed_repair_allowance`, `non_object_tool_arguments_are_repairable_invalid_calls` |
| Live/replay and restart parity | `malformed_tool_result_replays_with_the_same_provider_safe_arguments`, `recovered_malformed_tool_keeps_safe_arguments_and_consumed_repair_allowance`, `repair_allowance_survives_restart_after_a_new_request_epoch` |
| Canonical name repair, ambiguity and ceilings | `tool_name_case_and_underscore_repair_is_reported_in_durable_and_live_result`, `tool_name_repair_does_not_resolve_ambiguous_or_unadvertised_names`, `recovered_tool_name_correction_survives_a_checkpoint` |
| Store failures preserve item/recovery laws | `repair_reset_store_failure_closes_pending_tools_without_dispatch`, `repair_recovery_read_failure_leaves_the_checkpoint_recoverable` |
| Typed wire / legacy compatibility | `invalid_tool_call_data_has_a_typed_round_trip_without_changing_legacy_results` |
| Alias schema, effects, receipts and safety | `flat_filesystem_aliases_reject_mixed_shapes_unknown_fields_and_invalid_types`, `flat_edit_and_write_round_trip_matches_transactional_tools_and_receipts`, `flat_aliases_preserve_unread_stale_anchor_and_workspace_refusals`, `flat_filesystem_aliases_are_advertised_with_identical_routes_and_permission_defaults` |
| Anchor suggestions never mutate or expose redacted content | `anchor_miss_reports_nearest_whitespace_candidate_without_applying_it`, `anchor_miss_diagnostics_are_bounded_and_preserve_read_redaction` |

The old first-malformed-is-terminal test was explicitly replaced by the new
requested contract. Its no-dispatch and durable-failure assertions remain, and a
second-malformed terminal pin now covers the bounded failure path. This lane adds
no ignore attributes or platform gates and removes no test assertions to reach green.

An initial alias escape fixture failed because the outside file did not exist:
existing edit internals returned NotFound before their canonical boundary check.
The corrected fixture uses an existing sibling sentinel, retains the exact
WorkspaceBoundary assertion and verifies that the outside file is unchanged.

## Gates

All cargo commands use `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
Daemon/client suites also use `HAIDER_TEST_SIBLINGS_PREBUILT=1` after rebuilding
`haider-daemond` and `haider-cli --locked`. Disk is checked before every build;
the full gate stops its own cargo process tree below 700 MiB.

| Command / check | Result |
| --- | --- |
| `cargo build -p haider-daemond -p haider-cli --locked` | PASS; refreshed after final production edits. `haiderd` is a 198,001,824-byte arm64 Mach-O, above the 10 MiB floor. |
| `cargo test --workspace --no-fail-fast --locked -- --test-threads=4` | PASS on final rerun, exit 0. Initial pass exposed only the stale CLI request golden described above. |
| `cargo clippy -p haider-core -p haider-tools -p haider-protocol -p haider-daemon -p haider-daemond -p haider-cli --tests --locked -- -D warnings` | PASS, exit 0. |
| `cargo run -p xtask --locked -- test-count --update` | PASS; baseline 4766→4786, a net 20 added tests. |
| `cargo run -p xtask --locked -- check` | PASS; 4786 tests, 704 files scanned, nine existing files above the soft LOC cap. |
| `cargo fmt --all --check` | PASS. |
| `bash scripts/check-unsafe-counts.sh` | PASS; production 189, test 20. No unsafe source added. |
| `git diff --check` | PASS. |

Logs: `/tmp/toolrepair-siblings-final.log`, `/tmp/toolrepair-workspace.log`,
`/tmp/toolrepair-workspace-final.log`, `/tmp/toolrepair-clippy.log`,
`/tmp/toolrepair-test-count.log`, `/tmp/toolrepair-xtask-check.log`,
`/tmp/toolrepair-fmt-final.log`, `/tmp/toolrepair-fixture-refresh.log`.
This is the debug workspace gate. Existing live/manual test gates remain unchanged;
release timing thresholds and the supplied model benchmark were not executed.

Final workspace evidence: runtime 82/82, filesystem 31/31, CLI turnhygiene 9/9,
live-turn RPC 41/41. Every target succeeded with zero failed tests. Thirteen
pre-existing live/manual/environment-dependent tests remain ignored under their original gates; this
lane adds none. The existing 200,000-row TUI debug shape test also passed.

## CI error registry walk

The source registry was read in full, including #85/#86 (full workspace tests),
#87 (clippy tests), and #94/#95 (derived deadlines/keepalive). The existing unsafe
count guard exposed an inherited metadata mismatch: TUI test count 0 versus four
allocator wrappers already in `8430c886` and at this lane's base. Independent review
confirmed direct `System` forwarding, test-only scope and allocation-free thread-local
accounting. Only `ci/unsafe-counts.json`'s TUI test count advances 0→4; production
remains zero. This is the single outside-lane metadata correction; TUI source is
unchanged and exact-count enforcement remains active.

| Class | Review |
| --- | --- |
| 1 | checked — new ToolResultData variant and anchor diagnostic field; all constructors/matches searched |
| 2 | checked — ToolAccumulator requested_name constructors and close_malformed_tool_failure return/callers reviewed |
| 3 | checked — completion results retain ownership through durable/live projections |
| 4 | checked: none — no affected surface in this lane |
| 5 | checked: none — no affected surface in this lane |
| 6 | checked — two aliases added once to the canonical registry and consumers |
| 7 | checked: none — no manifest or lockfile dependency change |
| 8 | checked — surgical edits reread; no repeated broad sweeps |
| 9 | checked — clippy gate covers conditional/recovery paths |
| 10 | checked — new helpers are used on all platforms |
| 11 | checked — clippy gate covers conversions and result projections |
| 12 | checked — no function exceeds the existing argument budget |
| 13 | checked — clippy covers recovery state types |
| 14 | checked — additive typed data derives Eq and round-trips |
| 15 | checked — reverse traversal/ordinal fixture selection reviewed |
| 16 | checked: none — no affected surface in this lane |
| 17 | checked — no new lock held across await |
| 18 | checked — integration tests stay in existing test modules |
| 19 | checked — formatter and whitespace gate cover all changes |
| 20 | fixed — xtask regenerated the baseline 4766→4786; xtask check passes |
| 21 | checked — mandated 8 MiB Rust stack exported |
| 22 | checked: none — no process-global state introduced |
| 23 | checked: none — no affected surface in this lane |
| 24 | checked: none — no affected surface in this lane |
| 25 | checked: none — no affected surface in this lane |
| 26 | checked — diagnostic helper shared by Unix/Windows; Windows execution by inspection |
| 27 | checked: none — no affected surface in this lane |
| 28 | checked — full workspace uses four test threads; no new platform gating |
| 29 | checked: none — no affected surface in this lane |
| 30 | checked — fake provider fixtures have explicit response boundaries and terminal assertions |
| 31 | checked: none — no affected surface in this lane |
| 32 | checked: none — no affected surface in this lane |
| 33 | checked: none — no checked-in platform runner or serialization policy changed |
| 34 | checked: none — no dependency feature changes |
| 35 | checked — no ambiguous filesystem trait call introduced |
| 36 | checked — path fixtures retain owned temporary roots |
| 37 | checked — both platform anchor constructors include the new field |
| 38 | checked — recovery call-id sets/maps use matching string key types |
| 39 | checked — protocol/core/tools/daemon/daemond/CLI test consumers reviewed |
| 40 | checked: none — no feature-gated error conversion change |
| 41 | checked — new recovery tests are in-process; filesystem tests use short temp roots |
| 42 | checked — no new launch-timing assertion; existing cold-binary warmup behavior unchanged |
| 43 | checked: none — no affected surface in this lane |
| 44 | checked — actual workspace test outcome, not cargo check, determines test verdict |
| 45 | checked: none — no new unsafe code on any platform |
| 46 | checked: none — no affected surface in this lane |
| 47 | checked: none — no new walker or change to hidden-root/emptiness policy |
| 48 | checked — new tests are integration or existing declared test-module additions |
| 49 | checked: none — no affected surface in this lane |
| 50 | fixed — all platform size pins advance additively; percentage reduction law retained |
| 51 | checked: none — no affected surface in this lane |
| 52 | checked: none — no affected surface in this lane |
| 53 | checked: none — no affected surface in this lane |
| 54 | checked — ENV LAW and rebuilt siblings retained |
| 55 | checked — platform-neutral diagnostics return identical types |
| 56 | checked: none — no affected surface in this lane |
| 57 | checked — all exact registry consumers searched and updated together |
| 58 | checked — invalid results remain inline; CAS thresholds unchanged |
| 59 | checked: none — no affected surface in this lane |
| 60 | checked: none — no affected surface in this lane |
| 61 | checked — named tests assert effects, journal order, no dispatch and terminal cardinality |
| 62 | checked — helper return type changed only inside actor, every caller updated |
| 63 | checked: none — no affected surface in this lane |
| 64 | checked — disk monitor and Mach-O/size inspection prevent truncated-binary evidence |
| 65 | checked: none — no affected surface in this lane |
| 66 | checked: none — no affected surface in this lane |
| 67 | checked — fresh haider and haiderd built before daemon/CLI/client tests |
| 68 | checked — missing outside-file fixture distinguished from real boundary refusal |
| 69 | checked: none — no affected surface in this lane |
| 70 | checked: none — no affected surface in this lane |
| 71 | checked — sibling binaries are used by workspace integration tests; no release claim |
| 72 | checked — native discovery intentionally disabled for hermetic tests |
| 73 | checked: none — no fixed source byte-window pin added |
| 74 | checked — no user profile/credential reads in new tests |
| 75 | checked: none — no affected surface in this lane |
| 76 | checked — correction and invalid result survive live and durable model projection |
| 77 | fixed — inherited unsafe-count metadata corrected after independent source review |
| 78 | checked: none — no affected surface in this lane |
| 79 | checked: none — no affected surface in this lane |
| 80 | checked: none — no affected surface in this lane |
| 81 | checked — siblings refreshed after final production edits |
| 82 | checked: none — OAuth source/tests untouched |
| 83 | checked: none — hook runtime unchanged |
| 84 | checked: none — session hub/runtime unchanged |
| 85 | checked — full workspace requested to cover cross-crate registry consumers |
| 86 | checked — cargo check is not substituted for workspace tests |
| 87 | checked — affected-crate clippy includes --tests and -D warnings |
| 94 | checked: none — no new production deadline or timed wait added |
| 95 | checked: none — no new negotiated-connection wait added |

Classes 88–93 are not defined in the supplied source registry; no new class is invented.


## Independent review

Filesystem reviewer: shared internals, boundary/freshness/uniqueness safety,
redaction and all additive catalog changes reviewed without open code findings.
Actor reviewer: first/second error behavior, cross-epoch and deferred reset order,
name recovery, and store-failure lifecycle reviewed. Findings were fixed and given
regression pins. Final gate verdicts follow below.

Both independent reviewers returned **SHIP for the implementation** after inspecting
the final workspace and guard results. Neither has open findings. The overall
delivery verdict is **NO_SHIP** because the explicitly requested commit could not
be created under the current filesystem permissions.

## Commit delivery blocker

The explicit user request to commit was attempted through ordinary staging of only
the intended source, tests, fixture, and guard metadata. `git add` exited 128:

```text
fatal: Unable to create '/Users/rizzist/haider-run/haider-agent/.git/worktrees/lane-970-toolrepair/index.lock': Operation not permitted
```

The shared Git metadata lies outside this session's writable roots, and approval
escalation is unavailable. No staging, commit, or push occurred. The changes remain
in the lane worktree; the supplied briefs and turnperf directories remain untouched
and excluded. This is a delivery blocker, separate from implementation validation.

A scoped handoff patch is saved at `/tmp/toolrepair.patch`. It contains only the
21 intended source, test, fixture, metadata, and report files; it excludes all
supplied lane briefs and turnperf evidence. Exporting the patch does not stage or
commit any file.

NO_SHIP

## Merge-forward continuation — 2026-09-05

This continuation supersedes the original commit-delivery blocker above: the
current instruction explicitly requires an **uncommitted** resolved tree. The
actual lane HEAD is `8c339bf5721c64b7ff802268a46dbe21d80399e1`, an evidence-tracking
cleanup after `b91f0b21`; its only changes remove the supplied lane/turnperf files
from tracking. Those files remain untouched and unstaged in this continuation.

The requested fetch failed writing the external worktree `FETCH_HEAD`; the
fallback merge failed writing its external `ORIG_HEAD.lock`. The existing
`origin/wave-970` ref is the requested `b9c2a0475214102d1fb4c8d9c3ae3f480fd05fe4`.
A real `git merge --no-commit origin/wave-970` was therefore performed against the
same writable working tree using isolated Git metadata at
`/tmp/toolrepair-970-merge-93u6wqpm`, with read-only object alternates to the
original repository. This reproduced exactly the three reported conflicts.
The temporary index has no unresolved entries; the original repository's HEAD,
index and refs are unchanged. No commit or push was attempted or created.
The orchestrator owns recording the merge from the resolved tree/temporary index.

| File | Resolution and evidence |
| --- | --- |
| `crates/haider-daemon/src/permissions_core_tests.rs` | Kept 29 registered / 26 advertised tools, seven native descriptions, all platform-specific full-prefix pins, and the unchanged 30% reduction assertion. The runtime test measured the merged instruct pipe at **13,552 bytes**: wave **12,764 → 13,552 (+788)** = +597 schema/manual bytes +191 alias-description bytes; lane **13,409 → 13,552 (+143)** = turnbudget's stub schema. Description-free pin **12,862**, native descriptions **690**, macOS full prefix **19,736** (31.33% reduction). Turnbudget adds 367 full-schema bytes: Linux 19,785, Windows 19,735, other 19,730 are preserved offsets, by inspection. |
| `crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json` | Overwrote the conflicted file only through `UPDATE_FIXTURES=1 cargo test -p haider-cli --test turnhygiene_pin_tests provider_request_body_is_budget_independent_and_matches_the_golden_ledger -- --exact`. Passed 1/1. Reviewed the complete changed JSON line through exhaustive whole-object equality: removing only `spawn_subagent.request_budget` reproduces the lane parent; removing only `write`/`edit` definitions and their two manual lines reproduces wave. All other fields, ordering, existing native descriptions and actbias policy bytes remain identical. Canonical UTF-8 serialization is unchanged. Final 17,006 bytes; SHA-256 `5b511b74a5b01470ad7ccc7839b1253e1c77d4886ecd8ee667d2c09f1f8ef5e1`. |
| `test-baseline.txt` | Regenerated using `cargo run -p xtask --locked -- test-count --update`: lane 4,786 / wave 4,788 → merged **4,808**. Independent non-updating `target/debug/xtask test-count` confirms 4,808/4,808. This is the repository's source-marker count, separate from executed test totals. |

The HTTP request-body golden does not contain a `correlation` root field in either
parent. Turnid's additive correlation belongs to the journal/request-attempt
records. The merged `oneshot_run_golden.jsonl`, `run_jsonl_text_turn.jsonl`, and
`run_jsonl_tool_turn.jsonl` remain **byte-identical to wave-970**, retaining both
correlation and request-budget records. The two turnhygiene JSONL files use the
existing `<TS>`/`<N>` template normalization; they are intentionally not raw JSON.
No hand-edited HTTP correlation field or weakened golden assertion was introduced.

Automatically merged actor, prompt-history, worker, runtime tests, subagent tests,
and event-schema changelog preserve both sides. Independent reading confirms
repair-state recovery plus budget-state recovery, invalid-tool safe-object replay
plus budget-note replay, the pre-request budget check, and durable attempt commit
before transport. This continuation adds no production behavior or deadline.
The round-2 evidence's warning against unproven durable-boundary fusion is retained;
no latency, CPU, release-binary, or non-macOS execution claim is made.

Relevant citation audit: RawEnvelope cursor contract at
`docs/jsonl-run-contract-v1.md:15` remains correct. Older actor, schema and durable
boundary citations have drifted. The merged actor commits the provider attempt at
`crates/haider-core/src/actor.rs:3973` before opening transport at 4038. The old
unconditional-budget-projection behavior claim is stale (the actor's 3703 path is
gated); the old first-malformed-terminal claim is intentionally replaced by the
first-repair/second-terminal path at 5357–5389 and 7597–7650.

All Cargo invocations used `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
Sibling build `cargo build -p haider-daemond -p haider-cli --locked` passed before
subprocess tests, which used `HAIDER_TEST_SIBLINGS_PREBUILT=1`. The rebuilt
`haiderd` is an arm64 Mach-O of 198,287,680 bytes, above 10 MiB. Free space was checked
before every build and throughout the full gate; the 700 MiB floor was retained.

| Gate | Final result |
| --- | --- |
| `cargo test -q --workspace --no-fail-fast` | **PASS, exit 0**; 317 top-level result summaries: **5,191 passed, 0 failed, 13 pre-existing ignored, 0 measured, 0 filtered**. Six successful nested self-reexec summaries are excluded from these totals; eight nested child starts intentionally produced no separate summary. The independent verifier confirmed the accounting. Full command elapsed 1,900 seconds including compilation; the existing debug TUI 200k-row test passed in 772.85 seconds. |
| `cargo clippy --workspace --tests -- -D warnings` | **PASS, exit 0**, 2m30s; `--tests` and `-D warnings` included exactly. |
| Exact `permissions_core_tests::instruct_pipe_shrinks_the_advertised_wire_pack` | **PASS, 1/1**; runtime total 13,552 and full prefix 19,736 with the unchanged invariants. |
| `cargo fmt --all --check` | **PASS, exit 0**. |
| `bash scripts/check-unsafe-counts.sh` | **PASS, exit 0**; production 189/test 20. |
| Golden revalidation | Full non-blessing workspace run passed; final SHA-256 unchanged from regeneration. |
| Baseline check | **PASS**, source count 4,808 against baseline 4,808. |

The final workspace build also leaves an arm64 Mach-O `haiderd` of 198,262,736
bytes, independently rechecked after both gates. Windows/Linux execution and
release-only timing bounds remain by inspection/not executed as described above.

Evidence: `/tmp/toolrepair-970-workspace.log`, `/tmp/toolrepair-970-clippy.log`,
`/tmp/toolrepair-970-gate-summary.json`, `/tmp/toolrepair-970-bless.log`,
`/tmp/toolrepair-970-golden-review.log`, `/tmp/toolrepair-970-pipe-final.log`,
`/tmp/toolrepair-970-count-update.log`, `/tmp/toolrepair-970-count-check.log`,
`/tmp/toolrepair-970-fmt.log`, `/tmp/toolrepair-970-unsafe.log`.
The handoff diff from the original lane HEAD is
`/tmp/toolrepair-970-merge.patch`; it excludes the supplied lane/turnperf evidence.

### CI registry continuation walk

The source registry was reread in full. The original class 1–87 and 94–95 review
above remains applicable; the continuation rechecks those classes on the combined
tree, with the concrete updates below. The newer registry now defines **#88**;
only 89–93 remain undefined. No new CI-error class was encountered in the merge
resolution. No source/test ignore or platform-gating workaround was added.

| Class | Continuation review |
| --- | --- |
|1,2,3,4,5,6,39,62|checked — automatically merged API/constructor, ownership, cfg and test seams retain both parents; full workspace gates cover all consumers.|
|7,34,40|checked: none — no manifest, lockfile or feature changes from this resolution; sibling/xtask builds used `--locked`.|
|8,19|checked — surgical resolution reread; `cargo fmt --all --check` and both temporary-index diff whitespace checks pass.|
|9–18,35–38,55|checked — required workspace Clippy includes tests and denies warnings.|
|20|fixed — authoritative recount and non-updating check both 4,808.|
|21,54,64,67,81|checked — all ENV LAW values retained; rebuilt both siblings; binary format/size and disk floor checked.|
|22–33,41–49,51–53,56,58–61,63,65–66,68–75,78–84,94–95|checked: none — continuation introduces no production, platform, timeout, transport, lifecycle or release-policy changes; inherited behavior retained.|
|50,57|fixed — real merged pipe 13,552; native description accounting 690; platform offsets and 30% floor intact.|
|76|checked — wave's correlation and request-budget records preserved in JSONL and auto-merged consumers.|
|77|checked — unsafe-count guard passes with production 189/test 20; no unsafe-source or guard metadata change in this continuation.|
|85,86|checked — exact full workspace test run on the resolved combined tree; no check-only substitute.|
|87|checked — exact `cargo clippy --workspace --tests -- -D warnings` used.|
|88|fixed — wave merged using writable temporary metadata; provider golden generated by tooling, real pipe pin verified, baseline recounted, full merged-tree gates executed.|

Independent verifier returned **SHIP** after reviewing the merge seams, complete
golden comparison, measured pins, environment and exact gate logs. No finding
changed code, tests or verdict, and none was rejected as noise.

VERIFIER: findings=0 real=0 noise=0 — no findings
SHIP
