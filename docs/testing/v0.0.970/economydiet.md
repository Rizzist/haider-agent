# v0.0.970 economydiet

The default coding prefix is 57.59% smaller in the pinned AHRB reference
tokenizer. Model-result envelope overhead is 92.35% smaller. The durable
receipt contract and the actbias inspect → edit → verify contract and worked
example are retained.

## Claim audit and measurement

The initial tree was `368f093c6aab5c90b408d9f9206071eb74234c77`. Baseline binaries
were frozen and the primary measurement completed before product edits. Final
merge-forward is `7431f8e6e9500729362cc4eb3cfb2bbc62cf462a` (ceilingdecl); a fresh
clean upstream baseline reproduced every original economy scalar exactly. The
final table compares that upstream baseline with the merged lane candidate. The
historical ~11.7k tokens / ~23 root tools / ~11–12 KB manual claim has drifted:
this tree advertised 26 of 29 registered tools and had a 5,162-byte system
manual. The qualitative duplication and envelope findings still apply. The
historical 7.2× comparison to pi is not reasserted without a same-fixture pi run.

Both sides use the unmodified read-only AHRB economy adapter, the same task,
eight primary requests and 17 tool calls. Counts use exact common block
prefixes, independently for system/developer and tool blocks. The independent
analyzer cross-checks every complete request count, the stable-prefix total
and the context slope against the AHRB CLI. These are **AHRB reference tokens**,
not OpenAI billing tokens. Definitions, commands, tokenizer hash and binary
provenance are in [PROVENANCE.md](economydiet-evidence/PROVENANCE.md).

| Metric | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Fixed overhead, reference tokens | 14,222 | 6,031 | 57.59% |
| System-side reference tokens | 5,481 | 605 | 88.96% |
| Tool-side reference tokens | 8,741 | 5,426 | 37.92% |
| Policy bytes | 725 | 606 | 16.41% |
| System manual bytes | 5,162 | 0 | 100% |
| Canonical provider tool-schema array bytes | 10,705 | 6,383 | 40.37% |
| Combined canonical stable-prefix bytes | 16,698 | 7,062 | 57.71% |
| Native name/description/schema + manual byte pin | 13,552 | 5,670 | 58.16% |
| Tool-result content bytes/result | 1,280.59 | 264.76 | 79.32% |
| Envelope overhead bytes/result | 1,099.35 | 84.12 | 92.35% |
| Context-token curve slope | 4,584.67 | 2,103.17 | 54.13% |
| Task-proven wasted tool calls | 0 | 0 | unchanged |
| Default advertised tool count | 26 | 8 | 69.23% |

Independent side counts frame their own JSON object; the combined count frames
both sides once. Envelope overhead subtracts semantic output and includes the
retained truncation line on both sides. Raw numbers are in
[comparison-merged.json](economydiet-evidence/comparison-merged.json).

The bundled AHRB adapter labels **both** primary runs `terminal-without-effect`:
its normalization reads the started tool call before final arguments exist.
The official completion score is preserved. A separate join of actual captured
call arguments, model readback and external filesystem receipts passes all
seven effect checks before and after. This is a deterministic fixture result,
not a claim about real-model success. A supplementary native-first adapter
cannot complete the candidate capture because it injects an undeclared `route`
property now excluded by the preserved native schema. Its partial result is
not used for any before/after acceptance metric.

## Changes and contract pins

The shared provider projection unwraps daemon-owned process and filesystem
mutation receipts into output, a non-zero exit when present, and the required
truncation marker. Signal/timeout/cancellation diagnosis remains visible.
Legacy `exec`, remote process output and replay use the same boundary. User
output and file contents are opaque, including receipt-shaped JSON. Journal
`BoundedResult`, `/effects[n]`, IDs, digests, limits and truncation provenance
remain intact. Model byte accounting measures the resulting slim view.

Graph verification uses explicit `evidence_from` selectors to resolve this
run's terminal process or mutation facts in the journal. The store retains
authority, freshness, exit-status and slot-type validation; no model-supplied
testimony becomes verified provenance.

Worker changes are limited to catalog setup/restoration and this journal
evidence lookup. The merged turn ceiling and cap-before-provider-refresh
control flow are preserved; the lane does not change the adjacent budget,
retirement or workflow-continuation policies.

The default catalog is `list_tools`, `todo_write`, `fs_read`, `fs_glob`,
`fs_search`, `fs_write`, `fs_edit`, `process_exec`. `list_tools` with no filter
lists authorized names without promotion. A name or keyword filter describes
and promotes up to eight matches after the result commits. Exact names take
precedence. Promotion survives subsequent requests, turns, compaction,
provider refresh and workspace selection; forks start a new session scope.
Catalog order, rather than discovery order, determines schema bytes. Durable
restoration requires a correlated successful actor discovery result.

`HAIDER_TOOL_EXPOSURE=all` or a comma-separated list provides explicit
configuration. Lockdown and explicit allowlists remain the authorization
ceiling. Existing computer/mobile consent activation is retained; discovery
does not grant effect permission, and workspace changes reset consent.

`haider-system-v5` removes the redundant manual and the redundant context-order
sentence. The actbias contract, worked example and seven native action
descriptions have exact unchanged pins. Native schemas retain unique parameter
meaning and runtime constraints, including search bounds, edit uniqueness,
path destinations and `.git` exclusion. The 13,552 → 5,670 byte re-pin counts
that retained information deliberately.

Existing cache-fingerprint tests retain the order **system → system+tools →
+history**. New discovery pins prove unchanged system hashes, intentional
tool/history hash changes on promotion, and stable catalog bytes for repeated
or differently ordered discoveries. The AHRB system and tool blocks are each
byte-identical across all eight primary requests.

## Verification and landing

The merged workspace gate passes: **5,375 top-level tests plus 12 nested
subprocess probes**, zero failures and the same 13 pre-existing ignores as
upstream. The source-marker count is 4,944 → 4,969. Exact commands, exit codes
and durations are in [merged-gate-steps.json](economydiet-evidence/merged-gate-steps.json);
[workspace totals](economydiet-evidence/merged-workspace-totals.json) distinguish
nested reexecutions from top-level tests.

Clippy `--workspace --tests -- -D warnings`, `cargo fmt --all -- --check`,
`xtask test-count`, `xtask check`, the 66-test Python QA runner suite and
the source/authored-document `git diff --check` all pass. `xtask check` reports nine existing file-size
soft-cap warnings, with exit zero. Four additional PTY probe unit tests pass.
The complete staged whitespace check also includes raw terminal padding and
unmodified test logs: its warnings are confined to retained evidence paths, including
incoming upstream evidence. Those captured bytes are preserved and classified
in [whitespace-review.json](economydiet-evidence/whitespace-review.json).
The same development profile is used for both sides of the latency comparison;
the workspace's existing release-only rendering timing assertions are not
claimed as measured by this debug gate.

The final ABBA comparison passes all three latency criteria. Each row reports
wall median ± median absolute deviation, in milliseconds.

| Shape | Samples per side | Before, ms | After, ms | Delta, ms |
| --- | ---: | ---: | ---: | ---: |
| Warm single | 50 | 51.86 ± 2.60 | 47.22 ± 2.18 | −4.63 |
| Warm tool | 50 | 83.37 ± 4.69 | 74.82 ± 3.50 | −8.55 |
| One-shot | 42 | 107.42 ± 2.08 | 107.43 ± 2.10 | +0.01 |

All eight suites in A-B-B-A order pass the existing proof pins. Candidate
median must be at most baseline median plus the larger MAD; all three rows
satisfy that rule. Recorded load is 1.77–2.80, below the unchanged 3.0 limit.
All 108 known owned daemon PIDs are gone after cleanup. See
[ABBA results](economydiet-evidence/timing/abba.json) and
[cleanup](economydiet-evidence/timing/cleanup.json).

Two incomplete attempts remain recorded: an earlier load rejection and a
one-shot setup rejection caused by macOS's long inherited temporary path.
The final invocation pins `TMPDIR=/tmp` on both sides, matching the existing
QA launcher. No incomplete samples enter the comparison, and no accepted
regression was retried.

The unchanged final T0 retry passes **14/14**, with zero failures, skips or
environment blocks. Its report validates, `/update` exits cleanly, and all
16 owned-daemon cleanup checks pass. The prior failed run remains retained;
the successful retry does not establish its cause. See
[final T0 summary](economydiet-evidence/qa-t0-retry-summary.json) and
[validated report](economydiet-evidence/qa-t0-retry/qa-gate-t0-Syeds-MacBook-Air.local-20260905T090540Z.json).

The behavior pins include:

| Contract | Executable pins |
| --- | --- |
| Model output versus durable receipts | `process_model_envelope_is_output_and_only_nonzero_exit_with_journal_unchanged`, `filesystem_model_result_drops_receipt_and_preserves_journal_effects` |
| Truncation and replay | `typed_filesystem_truncation_preserves_savings_and_first_send_replay_footer_bytes`, `typed_large_tool_results_keep_legacy_model_prefix_suffix_cap_and_replay_bytes` |
| Core catalog, promotion and refresh | `default_coding_surface_and_catalog_read_do_not_promote`, `discovery_is_committed_before_the_next_request_advertises_it`, `provider_refresh_and_fallback_preserve_the_discovery_tier` |
| Authorization and byte stability | `discovery_cannot_promote_outside_catalog_or_from_rejected_results`, `discovery_promotes_once_in_catalog_order_and_keeps_policy_byte_stable`, `lockdown_turn_advertises_only_the_fixed_reduced_pack` |
| Native semantics and size | `native_action_parameters_preserve_the_former_manual_unique_constraints`, `search_and_mutation_tool_schema_descriptions_are_pinned`, `instruct_pipe_shrinks_the_advertised_wire_pack` |
| Existing prefix ordering | `cache_diagnostic_stable_prefix_has_identical_keyed_breakpoints`, `cache_diagnostic_old_length_proves_grown_prefix_contains_previous_entry` |

The merged fixture review covers all 116 JSONL/provider records. Regeneration
uses the existing update flags; the durable JSONL differences from upstream
are only system version `v4` → `v5`. Workspace receipts and the incoming
cap-before-provider-refresh behavior remain intact. See
[merged-golden-review.json](economydiet-evidence/merged-golden-review.json).

The initial T0 result was 12/14 and is retained. Its scripted headless
`request_input` fixture now explicitly selects that noncore tool, matching
the other scripted opt-ins without changing behavior assertions or deadlines.
The palette failures exposed two pre-existing probe defects: the monitor oracle omitted its
rendered controls, and the ANSI stripper recognized only BEL-terminated OSC
sequences. An ST-terminated attach notification consequently swallowed the
composer paint up to a later BEL notification. The fix accepts both terminal
terminators. All three original raw captures now retain the composer; a fresh
isolated attach succeeds in 1.313 seconds with unchanged assertions and
deadline, followed by verified daemon cleanup. Four probe unit tests cover
the parser and profile guard. No production TUI code changes. Evidence:
[attach resolution](economydiet-evidence/history-attach-diagnostic/resolution/result.json).

The first merged T0 run was 13/14: `/update` displayed its expected surface
but missed the original clean-exit deadline. All 16 owned-daemon cleanup rows
passed. The exact isolated activation then passed on both upstream baseline
and merged candidate, with exit zero and no signals. The unchanged manual
update path performs a blocking network check; the original failure mechanism
remains unproven because that run did not capture the OS wait status. No
production fix, assertion change or deadline change was made. The failed run
and comparison remain in [the update diagnostic](economydiet-evidence/update-exit-diagnostic/comparison.json).

Independent verifier findings: five real, zero noise. Fixed graph
provenance usability, legacy `exec` envelope projection, owned provider catalog
refresh/fallback, lost schema constraints, and exact renderer spacing in the
monitor QA oracle. Same-session workspace discovery
persistence was found and corrected by the implementing agent.

Builds and tests use the lane's 8 MiB stack, disabled discovery, deterministic
device identity, no incremental build/debug information, two build jobs and
prebuilt sibling binaries. Disk headroom is checked before builds. No OAuth
files, lane-common/brief evidence, or turnperf/turnperf2 inputs are changed.
Runtime validation is macOS arm64; Linux and Windows are by inspection.
The package version remains upstream's `0.0.969`; `v0.0.970` identifies this
lane's test evidence, with release versioning left to the release owner.

## Commit transport

The real worktree Git metadata is read-only under the workspace permission
policy. Source commit `e9aac6b5` and the merge/verification commit are recorded
in the writable Git directory `/private/tmp/economydiet-git`, whose worktree is
this lane. [economydiet.bundle](economydiet.bundle) carries those commits for
the repository owner. The working files contain the resolved merge from
`7431f8e6`; the original branch/index metadata remains unchanged. Commit
messages have no trailers, and nothing is pushed. Supplied lane briefs,
turnperf/turnperf2 evidence and frozen binaries are excluded from the commits.

Final acceptance: **SHIP**, with the retained transient T0 failure and AHRB
completion-score limitation described above.
