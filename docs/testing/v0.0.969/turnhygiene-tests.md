# turnhygiene — behaviour-preservation pins (v0.0.969)

Date: 2026-09-01
Base: `d750e09` (wave-969) on branch `lane-969-turnhygiene`
Scope: tests only. No product code changed. Every new pin passes on the
base tree; the implementation lane must keep every row below green.

The turn-hygiene lane removes redundant work on the WARM turn path (budget
projection skip, submit-response buffering, hook-discovery snapshot per run,
instruction snapshot cache, one append context per worker transaction, CAS
barrier skip for index-proven blocks, tool-path fast paths, JSONL-minimal
reducer, request-aware profile resolution, receiver-aware projections). None
of those items is allowed to change an observable outcome. The pins therefore
lock OUTCOMES — wire bytes, JSONL bytes, journal/replay parity, typed tool
results, files a hook writes, store readability — never internals.

## New tests

| Crate / file | Test | Kind |
| --- | --- | --- |
| `crates/haider-cli/tests/turnhygiene_pin_tests.rs` | `run_jsonl_text_turn_matches_the_normalized_golden` | process, golden `tests/fixtures/turnhygiene/run_jsonl_text_turn.jsonl` |
| same | `run_jsonl_tool_turn_matches_the_normalized_golden` | process, golden `run_jsonl_tool_turn.jsonl` |
| same | `replay_of_a_tool_call_turn_equals_the_live_run_scoped_jsonl` | process |
| same | `provider_request_body_is_budget_independent_and_matches_the_golden_ledger` | process + loopback OpenAI-compatible proxy, golden `provider_request_no_budget.json` |
| same | `resident_daemon_rediscovers_project_instructions_across_runs_and_cwds` | process + proxy, one resident daemon |
| same | `resident_daemon_discovers_a_hook_installed_between_runs_and_scopes_it_by_cwd` | process, one resident daemon |
| same | `custom_provider_binds_from_explicit_flags_a_model_selector_and_the_profile_default` | process + proxy |
| same | `jsonl_envelopes_reach_stdout_before_a_later_provider_delay_elapses` | process, timed stdout reader |
| same | `detached_run_journals_the_same_envelopes_as_an_attached_run` | process, `--start` + `--replay` |
| `crates/haider-tools/tests/process_tools_tests.rs` | `process_exec_keeps_stdout_and_stderr_apart_and_reports_a_nonzero_exit` | broker |
| same | `process_exec_reports_a_signal_killed_leader_as_a_signal_not_an_exit_code` | broker |
| `crates/haider-daemon/src/tasks_runtime_tests.rs` | `foreground_process_exec_projects_non_utf8_output_lossily_and_keeps_the_exact_digest` | in-process dispatcher + journal |
| `crates/haider-daemon/src/project_instructions_tests.rs` | `removing_the_directory_winner_between_turns_promotes_the_shadowed_agents_file` | in-process worker + FakeProvider |
| same | `sibling_workspaces_in_one_process_load_only_their_own_instructions` | in-process, two workers |
| `crates/haider-store/tests/provider_view_store_tests.rs` | `provider_views_with_preexisting_blocks_stay_readable_across_expiry_and_reopen` | store |
| `crates/haider-provider/tests/canonical_digest_golden_tests.rs` | `canonical_tool_definitions_digest_is_a_frozen_wire_law` | pure, literal BLAKE3 golden |
| same | `canonical_tool_definitions_digest_ignores_order_and_tracks_content` | pure |

Process pins follow the `cli_tests.rs` conventions: prebuilt sibling
`haiderd` (`HAIDER_TEST_SIBLINGS_PREBUILT=1`), hermetic `HAIDER_PROFILE_DIR`
plus machine `HOME`, `HAIDER_DISCOVERY_DISABLED=1`, one bounded retry only on
the transient cold-start exit 69, and every spawned daemon is stopped with
`haider daemon stop --json` (PID kill as the fallback) when the profile drops.
Proxy-backed runs set `HAIDER_AUTO_HERMETIC=0` — exactly what
`scripts/qa-gate/turnperf_support.py` does — because a custom no-auth provider
otherwise binds the automatic hermetic lockdown pack (7 tools, no project
instructions), which is not the warm path the lane measures.

### Golden normalisation

The goldens are byte-exact after one deterministic, documented rewrite of
identity fields only (`normalize_jsonl` / `normalize_body` in the test file):

- the canonical workspace path (JSON-escaped) becomes `<CWD>`; the shell
  command literal of the tool turn becomes `<CMD>`;
- every maximal lowercase-hex run of exactly 64 or 32 characters becomes
  `<H64>` / `<H32>` (session/run/event/item/effect identities, keyed cache
  hashes, argument/transcript/mutation digests, `prompt_cache_key`);
- every 13-digit decimal run becomes `<TS>` (`committed_at_ms`, item-id and
  effect-id clocks);
- the three byte-estimate counters (`input_tokens`, `used_tokens`,
  `stable_prefix_tokens`) become `<N>` because they scale with the workspace
  path length, which differs per machine. Their invariants (positive, equal
  across the start/complete pair) are asserted directly instead.

Everything else — every envelope, every field, field order, `seq` values,
`workspace_revision`, tool-result previews, effect summaries, usage scope
boundaries, terminal augmentation — is compared verbatim. Re-bless only after
reviewing the printed line diff: `UPDATE_FIXTURES=1 HAIDER_TEST_SIBLINGS_PREBUILT=1 cargo test -p haider-cli --test turnhygiene_pin_tests`.
The fixtures were blessed on macOS; the tool catalog is process-wide, but if
a platform-specific tool ever enters the catalog the body golden needs a
per-platform variant rather than a looser comparison.

## Behaviour → test → what breaks if it fails

Existing tests are cited by name; NEW marks a test added by this lane.

### (1) Provider-budget projection

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| With a budget configured, the projection and its enforcement outcome are unchanged (cost, tokens, time, unknown pricing, second request, usage reconciliation) | `run_budget_tests.rs`: `projected_first_request_over_cap_sends_zero_provider_requests`, `projected_token_budget_is_checked_at_the_same_preflight_seam`, `projected_second_request_over_cap_sends_exactly_one_provider_request`, `unknown_provider_pricing_fails_closed_before_the_request`, `unknown_pricing_is_named_even_when_a_token_projection_also_exceeds_its_cap`, `elapsed_time_is_checked_before_request_with_no_candidate_projection`, `an_unpriced_nonzero_run_fails_a_configured_cost_budget_closed`, `native_pdf_bytes_bind_the_token_cap_before_the_first_request`, `missing_actual_usage_fails_closed_after_the_request`; `cli_tests.rs`: `run_jsonl_unknown_pricing_has_one_distinct_budget_terminal_and_exits_77`, `daemon_time_budget_exhaustion_is_typed_and_exits_77`, `fast_final_usage_is_budget_checked_before_done`; `core_loop_e2e_tests.rs`: `workflow_hop_cost_cap_terminalizes_budget_before_request_two` | A budgeted run sends a request it should have refused, names the wrong reason, or spends past its cap |
| Without a budget the outgoing chat request is byte-identical to today's for a fixed prompt | NEW `provider_request_body_is_budget_independent_and_matches_the_golden_ledger` (golden `provider_request_no_budget.json`) | The skip changes the system message, tool catalog serialisation, message projection or any header-equivalent field |
| A `--max-tokens` or `--max-time` budget never changes the request bytes; the warm second request equals the cold first one | NEW same test (four recorded bodies compared pairwise) | The projection or a cached snapshot leaks into the wire body |
| Footprint, cache-attempt and usage projections in the journal are identical with and without a budget (only the declared `budget` differs) | NEW same test (normalised journal parity) | Skipping the projection changes the estimated counters or drops the `context_footprint_v1` / `cache_request_attempt_v1` items |

### (3) Hook discovery

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| A hook installed between two runs is discovered by the next run and fires; retained hookless facts replay once the hook exists | NEW `resident_daemon_discovers_a_hook_installed_between_runs_and_scopes_it_by_cwd`; `hook_dispatch_peak_rss_tests.rs`: `zero_hooks_retain_without_decode_then_post_install_replay_fires_old_event` | A per-daemon snapshot never sees the new `hooks.json`, or the retained `user_message` row is acknowledged instead of replayed |
| A hook that fires today fires with its JSON payload (`event`, `session`, `run`, `mode`, text) | NEW same test; `user_message_hook_tests.rs`: `user_message_hook_fires_for_headless_and_rpc_submissions_identically`; `hooks_tests.rs`: `committed_user_message_hook_projection_is_surface_neutral`, `text_bounded_with_truncated_flag`, `attachment_metadata_never_carries_bytes` | The hook input loses fields or the fire is skipped |
| Discovery differs correctly across cwd within one daemon lifetime (A's hook never sees B's session and vice versa; alternating A→B→A works) | NEW same test; `hooks_tests.rs`: `subscriber_identity_is_scoped_to_workspace_cwd` | A snapshot keyed on the daemon or the last cwd fires a foreign hook or misses its own |
| One discovery stamp per committed batch (current cost pin) | `hooks_tests.rs`: `committed_batch_computes_discovery_stamp_once` | Note: this pins the present cache-hit cost; a per-run snapshot may lower it but must keep every row above |
| Fire-time re-verification and edit-revocation are unchanged | `hooks_tests.rs`: `fire_time_reverification_refuses_a_swapped_pinned_definition`, `digest_change_revokes_trust_before_fire`, `hooks_list_reports_revoked_by_edit_as_wire_truth`, `trust_workspace_pins_its_first_digest_across_restart` | A cached definition executes after its bytes changed |

### (4) Project-instruction snapshot

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| Editing an instruction file between two turns changes the next request's session content (same-length edit, real daemon, proxy ledger) | NEW `resident_daemon_rediscovers_project_instructions_across_runs_and_cwds`; `project_instructions_tests.rs`: `one_pinned_logical_turn_sees_one_snapshot_and_edits_apply_next_turn`, `loaded_fact_is_durable_omitted_change_only_and_not_a_broker_effect` | A size/mtime-only or unbounded cache serves stale bytes |
| Ancestor-directory instructions are found from a deep cwd; nearest composes last | NEW same CLI test (three levels deep); `project_instructions_tests.rs`: `nearest_instructions_compose_last_and_haider_wins_within_directory`, `total_cap_preserves_nearest_files_and_composes_them_last`, `ancestor_depth_cap_reaches_the_prompt_with_a_counted_machine_marker` | An O(ancestors) walk stops early or reverses precedence |
| A removed file stops being included; the shadowed `AGENTS.md` is promoted when `HAIDER.md` is removed | NEW `removing_the_directory_winner_between_turns_promotes_the_shadowed_agents_file`; NEW CLI test above; `loaded_fact_is_durable_omitted_change_only_and_not_a_broker_effect` | Invalidation misses a delete, or the per-directory winner is cached past its removal |
| The snapshot is keyed on the session's canonical cwd, never process-wide | NEW `sibling_workspaces_in_one_process_load_only_their_own_instructions`; NEW CLI test (two workspaces on one daemon); `sibling_sessions_share_the_base_and_emit_session_context_after_it` | One workspace's bytes leak into another's request |
| One snapshot per logical turn (tool rounds/retries reuse it) | `one_pinned_logical_turn_sees_one_snapshot_and_edits_apply_next_turn`, `recovery_rereads_and_journals_a_fresh_same_run_fact_on_digest_change` | A mid-turn edit reaches a tool round, or recovery trusts a stale digest |
| Change-only `project_instructions_loaded` facts, prompt-omitted, on the accepted branch | `loaded_fact_is_durable_omitted_change_only_and_not_a_broker_effect`, `loaded_fact_keeps_the_accepted_named_branch_coordinate`; NEW winner-flip test (exactly three facts) | The fact is emitted every turn, rendered into the prompt, or missing after a change |

### (5)/(6) Journal appends, replay parity, provider-view CAS

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| Typed events reconstructed from the journal equal the live stream across a tool-call turn, exactly one terminal, run-scoped, `terminal_kind` is a JSONL-only augmentation | NEW `replay_of_a_tool_call_turn_equals_the_live_run_scoped_jsonl`; `cli_tests.rs`: `replay_is_a_read_only_exact_durable_projection`, `replay_is_sealed_at_terminal_before_late_same_run_task_facts`; `upstretry_e2e_tests.rs`: `bounded_429_ladder_terminalizes_before_caller_deadline` | A batched append reorders, drops or duplicates an envelope; a second terminal appears |
| The whole warm tool-turn journal is byte-stable (field-by-field, ordered) | NEW `run_jsonl_tool_turn_matches_the_normalized_golden`, NEW `run_jsonl_text_turn_matches_the_normalized_golden` | Collapsing cursor/GC statements or decoding payloads once changes any appended payload |
| Journal bytes survive reopen; store replay equals appended bytes | `store_tests.rs`: `append_read_and_reopen_replay_are_byte_identical`; `sqlite_store_tests.rs`: `real_store_reopen_replays_the_identical_envelope_bytes` | A validated-once append context writes different bytes |
| Provider-view objects for a request whose blocks all already existed stay readable and consistent, survive the older request's expiry and a store reopen; partially shared requests likewise | NEW `provider_views_with_preexisting_blocks_stay_readable_across_expiry_and_reopen`; `provider_view_store_tests.rs`: `expired_provider_view_sweep_preserves_live_shared_blocks`, `provider_view_index_and_full_attempt_batch_commit_or_rollback_together`, `provider_view_request_ordinal_survives_session_id_reuse` | Skipping the directory barrier drops the index row or its durability; a shared block is freed while still referenced |
| Trailing barrier ordering when a block is NEW (blob durability precedes its index reference) | `src/provider_view_store_tests.rs`: `provider_view_persist_uses_one_trailing_barrier_before_indexing` | Note: this pins exactly one barrier for new blocks; it deliberately does not cover the all-index-proven case, which the NEW store test covers by outcome |
| Live vs sealed replay high-water parity; contiguity at every replay/live boundary | `session_hub_tests.rs`: `sealed_replay_skips_only_durable_item_deltas_and_preserves_high_water`, `replay_live_barrier_is_contiguous_at_every_forced_boundary`, `slow_client_is_lagged_and_store_resume_is_contiguous` | A buffered submit-response path publishes out of order |

### (7) Tool path

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| `process_exec` keeps stdout and stderr apart in deltas and inline output, reports a nonzero exit as `Failed` + `exit_code`, digest over exact capture-order bytes, effect outcome `Ok` | NEW `process_exec_keeps_stdout_and_stderr_apart_and_reports_a_nonzero_exit`; `live_turn_rpc_tests.rs`: `w4a2_exec_is_cas_gated_streams_output_and_grants_only_the_exact_shape` | Folding capture into the supervisor merges streams, drops bytes, or re-hashes a projection |
| A signal-killed leader reports `signal`, not an exit code or a limit | NEW `process_exec_reports_a_signal_killed_leader_as_a_signal_not_an_exit_code` | Exit classification changes |
| Large output spills while streaming; the hard cap terminates the group and reports the ledgered limit | `process_tools_tests.rs`: `output_flood_spills_while_streaming_and_completes`, `hard_output_cap_terminates_the_process_group_and_reports_the_ledgered_limit`; `core_loop_e2e_tests.rs`: `tool_calls_execute_and_continue_over_real_rpc` | Output capture loses bytes or the cap stops firing |
| Non-UTF-8 bytes: exact bytes and digest at the broker; lossy `U+FFFD` projection at the model boundary with the raw count/digest intact and one process signal | `process_exec_streams_exact_bytes_freezes_overflow_and_journals_four_phases`; NEW `foreground_process_exec_projects_non_utf8_output_lossily_and_keeps_the_exact_digest` | The projection or the signal starts describing decoded text |
| Wall timeout terminates the group and reports `WallTimeout` | `wall_timeout_terminates_the_process_group_and_reports_the_ledgered_limit` | A reusable executor loses the deadline |
| One `process_signal_recorded` and one workspace-mutation outcome per foreground tool call; receipts stay bounded | `tasks_runtime_tests.rs`: `foreground_process_exec_is_unchanged_and_journals_no_task_facts`; `process_tools_tests.rs`: `workspace_mutation_fact_fires_only_when_process_changes_the_tree`, `prior_after_receipt_is_the_next_sequential_before_receipt`, `overlapping_process_receipts_are_conservatively_unknown_for_both_commands`, `process_exec_runs_and_returns_output_with_a_huge_workspace_file`; `workspace_receipt_tests.rs` (all 11); NEW tool-turn golden (effect outcome + signal envelopes) | A reused receipt executor skips, duplicates or mis-orders receipts |
| No-graph dispatch admits any command; graph-bound typed executors keep their CLI fence; tool pack cache rebuilds on every revision change | `worker_tool_catalog_tests.rs`: `turn_tool_pack_cache_rebuilds_when_{provider,grant,lockdown,registry,mode}_revision_changes`, `approval_retry_cache_reuses_typed_operation_and_fences_full_call_identity`; `worker_typed_workflow_boundary_tests.rs`: `native_typed_workflow_on_generic_child_fails_closed_without_graph_evidence` | A revision-cached fast path serves a stale pack or drops the fence |
| Fragmented tool-call identity and argument-shape rejection are unchanged | `cli_tests.rs`: `run_jsonl_fragmented_tool_call_keeps_one_call_identity_and_cursor`, `run_model_tool_argument_shape_error_is_rejected_and_continues` | The tool path loses the provider call id or the rejection carrier |

### (8) JSONL output

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| Byte-identical `haider run --jsonl` for a fixed fake-provider script — every field, every event, order — for a one-request turn and a tool turn | NEW `run_jsonl_text_turn_matches_the_normalized_golden`, NEW `run_jsonl_tool_turn_matches_the_normalized_golden` | A minimal reducer drops or reorders a record, or changes the terminal augmentation |
| Acceptance line first, LF framing, contiguous cursor, exactly one typed terminal | `cli_tests.rs`: `run_jsonl_announces_acceptance_before_lf_framed_envelopes`, `run_jsonl_cancelled_has_130_exit_and_terminal_envelope`, `run_jsonl_timeout_has_one_distinct_timeout_terminal`; `run.rs` tests: `jsonl_adapter_writes_accepted_before_any_envelope`, `jsonl_adapter_preserves_provider_wait_and_run_failed_payload_shapes` | Framing or cursor law regresses |
| Envelopes reach stdout within a bounded time of commit — the first text delta is on stdout before a 1.5 s provider delay elapses; no envelope arrives before its commit or more than 10 s after it | NEW `jsonl_envelopes_reach_stdout_before_a_later_provider_delay_elapses`; `run.rs` tests: `jsonl_adapter_flushes_a_queued_batch_at_acceptance_and_terminal` | A flush deadline or batching holds records until the terminal |
| Lossless delivery under consumer back-pressure | `run_jsonl_replays_every_envelope_to_a_slow_pipe_consumer` | The minimal reducer drops under lag |
| Steady-state thread count of a staged run | `staged_run_with_resident_daemon_has_two_steady_state_threads` | Removing the last per-run output worker changes the count — update deliberately |

### (9) Profile resolution

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| Explicit `--provider/--model` binds a custom provider with no profile default configured; the wire model is verbatim | NEW `custom_provider_binds_from_explicit_flags_a_model_selector_and_the_profile_default`; `cli_tests.rs`: `run_jsonl_accepts_explicit_fake_provider_and_model`, `configured_custom_model_reaches_chat_wire_verbatim_despite_catalog`; `headless_run_tests.rs`: `configured_provider_model_selector_reaches_create_as_bare_wire_id` | A request-aware path refuses or rewrites explicit selections |
| A `provider/model` selector alone binds both | NEW same test | Selector splitting regresses |
| Defaults apply when flags are absent (`config.json` `default_model` selector binds provider and model) | NEW same test; `cli_tests.rs`: `flagless_run_without_an_active_account_exits_65_with_remedy`; `headless_run_tests.rs`: `flagless_bootstrap_creates_on_active_provider_and_published_default_model`, `account_only_bootstrap_uses_that_accounts_daemon_default_model`, `daemon_resolved_default_without_published_model_is_typed`, `flagless_bootstrap_without_active_account_is_typed`; `profile_tests.rs`: `model_precedence_is_env_then_config_then_packaged` | Not materialising defaults for explicit runs must not stop materialising them for flagless runs |
| An unknown provider still yields the typed `invalid_argument` refusal (exit 76, `unsupported session provider`) | `cli_tests.rs`: `unknown_run_provider_surfaces_daemon_create_refusal`, `run_jsonl_bootstrap_failures_always_end_in_a_typed_error_record` | The refusal moves, loses its code, or a CLI allowlist returns |
| Bootstrap failures (corrupt `config.json`, unsupported effort, missing attachment) end in one typed JSONL error record | `run_jsonl_bootstrap_failures_always_end_in_a_typed_error_record` | Note: the corrupt-config case is flagless; with explicit flags the lane may legitimately stop reading `config.json` — that is unpinned either way |

### (10) Projections, observers, hashing, outbox, expiry

| Behaviour | Test(s) | What breaks if it fails |
| --- | --- | --- |
| With no observer attached the journal contents are unchanged (a detached `--start` run journals exactly what an attached run does) | NEW `detached_run_journals_the_same_envelopes_as_an_attached_run` | A receiver-aware projection leaks a listener-only fact into, or drops one from, the durable record |
| An observer attaching mid-run/after-restart receives the catch-up it gets today (store replay → `AttachCaughtUp` → buffered → live; pending menu learned from replay; digest rebuilt byte-identical) | `session_hub_tests.rs`: `replay_live_barrier_is_contiguous_at_every_forced_boundary`, `attachment_after_menu_opened_learns_pending_menu_from_replay`, `full_internal_catch_up_receiver_reregisters_and_resumes_from_store`, `many_concurrent_cursors_each_receive_their_exact_suffix`, `summaries_report_turns_and_tokens_for_unattached_sessions`, `metadata_only_observe_shares_authoritative_fields_and_roster_truth`; `rpc.rs` tests: `ready_eviction_rebuilds_byte_identical_digest_from_journal`, `concurrent_observers_of_evicted_session_share_one_rebuild`; `observe_cli_tests.rs`: `watch_streams_are_lf_framed_raw_envelopes_and_tolerate_additive_kinds` | A no-listener fast path forgets to re-arm projections when a listener appears |
| Canonical JSON → BLAKE3 tool-pack digest is a frozen wire law; key order is ignored, array order and content are semantic | NEW `canonical_tool_definitions_digest_is_a_frozen_wire_law`, NEW `canonical_tool_definitions_digest_ignores_order_and_tracks_content`; `lib.rs` tests: `streaming_tool_digest_matches_legacy_canonical_dom_bytes`; `actor.rs` tests: `cm2a_system_and_tool_digests_are_stable_across_append_only_history`; `graph.rs` tests: `m2b_node_wire_and_ship_loop_digest_are_legacy_stable` | Hashing canonical JSON straight into BLAKE3 changes the bytes that are hashed, rotating every prompt-cache identity |
| Trace-off output identity | NEW JSONL and body goldens (trace is off by default); `lib.rs` tests: `turn_trace_transaction_ordinal_is_shared_by_batch_identity` | See needs-a-hook: the allocation-free claim itself is untestable today |
| Hook engine facts never re-enter the outbox; retained rows replay exactly once; batched ACK is one durable transaction | `hook_dispatch_outbox_tests.rs`: `hook_engine_facts_do_not_reenter_the_dispatch_outbox`, `batched_acknowledgement_is_one_atomic_durable_transaction`, `hook_dispatch_outbox_is_atomic_persistent_and_idempotently_acknowledged`; `hooks_tests.rs`: `live_drain_cycle_acknowledges_handled_rows_in_one_batch`, `recovery_replays_exactly_the_unacknowledged_rows`; NEW hook CLI test (a retained `user_message` replays after install) | Dropping rows for always-acknowledged kinds must not drop a kind a later hook can match |
| Expired provider views are swept, live shared blocks survive, sweeps re-arm on the retention boundary | `provider_view_store_tests.rs`: `expired_provider_view_sweep_preserves_live_shared_blocks`, `expired_provider_view_sweep_handles_more_than_one_batch`; `src/provider_view_store_tests.rs`: `due_sweep_expires_at_the_retention_boundary_and_rearms`, `consecutive_persists_schedule_at_most_one_sweep_per_count_window`, `default_expiry_preserves_the_seven_day_retention_policy`; NEW store test | Note: the in-crate schedule tests pin the sweep running synchronously inside persist; moving it off the critical path relocates those pins but must keep the outcome rows |

## Needs a hook — pin after implementation

These outcomes cannot be observed from outside today without a product seam;
they are listed so the implementation lane adds the seam and the pin together.

1. **Trace-off path is allocation- and clock-free** (item 10).
   `turn_trace_enabled()` is a process-wide `OnceLock`, so a test cannot flip
   it; the only pin is the doc comment. Needs either an injectable override or
   a child-process capture asserting no `haider.turn` records without
   `HAIDER_DAEMON_TRACE=1`.
2. **Budget projection is skipped when no budget is set** (item 1). The
   goldens prove the outcome is unchanged; proving the message clone and tool
   catalog serialisation no longer happen needs a test-only counter on the
   estimator (or `FakeProvider`-level request accounting in the daemon path).
3. **Receiver-aware commit projections do no clone/deserialize without a
   listener** (item 10). `ObserveDigestCache::stats()`/`contains()` are
   `#[cfg(test)]` in `session_hub/rpc.rs`; the direct pin (append without
   `cached_observe_snapshot` → `stats() == (0,0,0)`, then observe once → fold
   advances) belongs beside `ready_eviction_rebuilds_byte_identical_digest_from_journal`
   once the lane settles the seam.
4. **No hook-outbox rows for always-acknowledged kinds** (item 10). Today
   `node_committed` / `item_tool_call` rows exist and are acknowledged without
   decode; the visible result is identical either way. After implementation,
   pin `has_pending_hook_dispatches` immediately after such an append.
5. **Expiry sweep off the provider critical path** (item 10). A bounded wait
   for "expired views are gone eventually" needs a sweep-completed signal
   (counter or status field); today the sweep is synchronous and pinned by
   the in-crate schedule tests.
6. **Real 3 ms JSONL flush deadline** (item 8). The writer (`adapt_events_to`)
   is private with an inline test module in `run.rs`; a deterministic
   writer-level pin needs a `FlushCountingWriter` case that parks on an empty
   channel for longer than the deadline. The process-level bound above covers
   the observable outcome.
7. **Move-only startup plan / no per-run output worker** (item 9). Internal;
   `staged_run_with_resident_daemon_has_two_steady_state_threads` will need a
   deliberate update if the thread count changes.
8. **Hook-discovery snapshot per logical run** (item 3). The NEW CLI test pins
   the outcome; `committed_batch_computes_discovery_stamp_once` pins the
   present per-batch cost and may be re-baselined downward, never upward.

## Verification

Command environment for every run:
`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 HAIDER_TEST_SIBLINGS_PREBUILT=1 CARGO_BUILD_JOBS=2`
after `cargo build -p haider-daemond --bin haiderd -p haider-cli --bin haider`.

Baseline: `test-baseline.txt` 4351 → 4368 (17 new tests, `cargo run -q -p xtask -- test-count --update`).

Per-crate `cargo test -p <crate>` (verbatim result lines of the binaries that
carry new pins, plus the crate totals that must stay green):

- haider-cli: `tests/turnhygiene_pin_tests.rs` → `test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.11s`; unittests `59 passed`, `observe_cli_tests` `71 passed`, `session_config_cli_tests` `65 passed`, `update_cli_tests` `71 passed`, `update_restart_tests` `66 passed`, `update_tests` `82 passed`, `status_discovery_smoke_tests` `4 passed`, `projection_completeness_tests` `2 passed`, `release_workflow_tests` `1 passed`, `cli_tests` `120 passed`, `autospawn_tests` `10 passed`, `wc_export_tests` `20 passed`; exit 0. The nine pins were additionally run three consecutive times in isolation (`9 passed` each) to prove the goldens are stable across daemons.
- haider-tools: `tests/process_tools_tests.rs` → `test result: ok. 30 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.88s` (the two `1 passed; 29 filtered out` lines above it in the log are that binary re-entering itself for its own process-group cases); unittests `81 passed; 0 failed; 1 ignored`; every other tools binary green; exit 0.
- haider-daemon: unittests → `test result: ok. 919 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 17.20s` (includes `tasks_runtime_tests::foreground_process_exec_projects_non_utf8_output_lossily_and_keeps_the_exact_digest`, `project_instructions_tests::removing_the_directory_winner_between_turns_promotes_the_shadowed_agents_file`, `project_instructions_tests::sibling_workspaces_in_one_process_load_only_their_own_instructions`); `session_hub_tests` `103 passed`; exit 0. The three ignored tests are pre-existing.
- haider-store: `tests/provider_view_store_tests.rs` → `test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.17s`; `store_tests` `28 passed`, `graph_tests` `65 passed`, every other store binary green; exit 0.
- haider-provider: `tests/canonical_digest_golden_tests.rs` → `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s`; unittests `212 passed`; live-credential binaries stay `ignored` as before; exit 0.

`cargo clippy -p <crate> --tests -- -D warnings`: haider-store, haider-provider, haider-tools, haider-daemon, haider-cli all exit 0 (one first-pass `await_holding_lock` finding in the new tools test was fixed by scoping the guard; re-check exit 0). `cargo fmt --all -- --check`: clean.

Every daemon the pins spawn is stopped by `haider daemon stop --json` in
`TestProfile::drop`; no `haiderd` remained under the test homes after the
sweep.
