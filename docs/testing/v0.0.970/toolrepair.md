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
