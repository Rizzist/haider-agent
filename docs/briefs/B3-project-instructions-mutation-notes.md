# B3 project-instructions mutation notes

Every mutation below has a runtime observer. “Expected RUNTIME failure” means
the named test fails by assertion, timeout, or typed protocol/store error; a
compile-only failure is not the claimed evidence.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Add delimiters for an empty walk, change the established policy body, or retain `haider-system-v1`. | `haider-daemon::project_instructions_tests::empty_walk_composes_byte_identical_v1_body_with_v2_version` | The exact prompt differs anywhere outside the pinned v2 version line. |
| Compose cwd-first, let `AGENTS.md` beat a readable `HAIDER.md`, or retain both files from one directory. | `nearest_instructions_compose_last_and_haider_wins_within_directory` | The ordered paths/content or composed precedence changes. |
| Read an unbounded file, split a UTF-8 scalar, omit the marker, or digest bytes other than the effective body. | `per_file_cap_truncates_at_utf8_boundary_with_marker` | The body exceeds 48 KiB, is invalid UTF-8, lacks `[truncated]`, or its digest differs. |
| Spend aggregate budget ancestor-first or remove the 96 KiB cap. | `total_cap_preserves_nearest_files_and_composes_them_last` | A nearer file truncates before an ancestor, order changes, or total contributed bytes exceed the cap. |
| Canonicalize through a symlinked parent, follow a symlinked candidate, or walk past root/depth bounds. | `upward_walk_refuses_symlinked_parents_and_stops_at_root` | A noncanonical workspace contributes policy or a root walk yields more than its single directory winner. |
| Reload on a provider retry/tool round, or keep one snapshot across two accepted logical turns. | `one_pinned_logical_turn_sees_one_snapshot_and_edits_apply_next_turn` | Requests within the first turn differ, or the next turn does not see the edit. |
| Omit/retype the fact, render it into prompt replay, journal unchanged snapshots, retain stale nonempty state after removal, or route loading through `EffectBroker`. | `loaded_fact_is_durable_omitted_change_only_and_not_a_broker_effect` | Fact count/run coordinates/render/digest/bytes differ, the empty transition is absent, or an `Effect` row appears. |
| Drop the accepted named branch from the raw fact or substitute a mutable/current branch. | `loaded_fact_keeps_the_accepted_named_branch_coordinate` and `haider-store/tests/project_instruction_tests.rs::worker_append_rejects_project_instruction_fact_on_the_wrong_branch` | The branch turn has no correctly stamped fact, or the store accepts a wrong-branch fact. |
| Reject the additive fact because it is absent from core `EventPayload`, or accept malformed lookalikes. | `haider-store/tests/project_instruction_tests.rs::{worker_append_accepts_project_instruction_fact_for_an_active_run,worker_append_rejects_malformed_project_instruction_fact}` | A valid raw fact cannot commit/preserve, or malformed `files` commits. |
| Trust pre-crash journal semantics, skip the recovery re-read, duplicate a matching fact, or fail to append a changed same-run fact. | `recovery_rereads_and_journals_a_fresh_same_run_fact_on_digest_change` | Recovery sends the old body or the same run does not end with exactly one corrected digest fact. |
| Exclude instructions from the W7 estimate or rebuild manual-compaction fit policy without them. | `footprint_and_manual_compaction_fit_include_instruction_bytes` | Direct estimation does not grow or the durable reset footprint differs from the exact composed-prompt estimate. |
| Remove/reorder fact wire fields or make the additive kind decode as core `EventPayload`. | `haider-protocol/tests/golden_tests.rs::golden_project_instructions_loaded_fact`, `raw_envelope_tolerates_unknown_payload`, and `haider-store/tests/store_tests.rs::raw_envelope_preserves_unknown_payload_kinds` | The golden differs, round-trip fails, or an older raw reader no longer preserves an unknown kind. |

The loader uses daemon-owned descriptor-relative reads only. No model tool
call is synthesized and no effect-journal lifecycle is entered. Supplemental
facts stay outside the exhaustive core payload enum, matching B2a's additive
`BranchEventPayload` pattern so `haider-tui` remains untouched.
