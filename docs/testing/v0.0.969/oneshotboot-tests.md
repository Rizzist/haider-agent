# v0.0.969 `oneshotboot` — behaviour-preservation pins

Scope: the one-shot daemon boot path (fresh profile, `HAIDER_RUN_DAEMON_IDLE_TTL_MS=0`,
`haider run` against a fake provider) and every piece of profile state a LATER
daemon on the same profile depends on. The implementation lane may change how
the boot path works; it must keep every test below green. Pins observe
contract-visible outcomes only (JSONL bytes, JSON documents, typed exit codes,
files a later daemon reads), never internals.

New in this lane:

| Test | Location |
|---|---|
| `one_shot_run_state_is_visible_to_a_later_persistent_daemon` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `one_shot_jsonl_stream_matches_the_normalized_golden` | `crates/haider-cli/tests/oneshot_boot_tests.rs` (+ `tests/fixtures/oneshot_run_golden.jsonl`) |
| `one_shot_boot_publishes_a_nonempty_daemon_log_and_bounds_log_history` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `fresh_daemon_lockdown_status_reports_defaults_without_a_policy` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `fresh_daemon_reconciles_seeded_lockdown_quota_before_its_first_command` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `fresh_profile_models_catalog_matches_the_golden` | `crates/haider-cli/tests/oneshot_boot_tests.rs` (+ `tests/fixtures/models_fresh_profile.json`) |
| `custom_provider_delta_survives_daemon_restart_beside_the_builtin_catalog` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `persistent_daemon_cache_diagnostic_key_is_durable_across_restart` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `one_shot_attachment_is_durable_in_the_profile_cas` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `fresh_profile_status_is_typed_without_a_daemon_and_reports_the_build_version_with_one` | `crates/haider-cli/tests/oneshot_boot_tests.rs` |
| `fresh_daemon_first_loom_list_returns_the_two_seeded_defaults_across_restart` | `crates/haider-daemond/tests/core_loop_e2e_tests.rs` |
| `attachment_written_by_the_first_daemon_is_projected_by_the_next_daemon` | `crates/haider-daemond/tests/core_loop_e2e_tests.rs` |

Golden fixtures are compared byte-for-byte after normalization (ids, timestamps,
digests, token estimates). Regenerate ONLY with `HAIDER_ONESHOT_GOLDEN_UPDATE=1`
after a reviewed contract change; a plain run never writes.

## Behaviour → pin → what breaks if it fails

Item numbers refer to the implementation lane's change list.

| Behaviour (contract-visible) | Pinned by | If it fails |
|---|---|---|
| (8,9) A TTL=0 `haider run` returns only after its owned daemon's checked exit: `lock.owner` gone, process gone, profile lock free, socket removed. | NEW `one_shot_run_state_is_visible_to_a_later_persistent_daemon`, `one_shot_jsonl_stream_matches_the_normalized_golden`, `one_shot_attachment_is_durable_in_the_profile_cas` (`assert_daemon_gone_now`); existing `cli_tests::one_shot_reaps_only_the_daemon_it_spawned_on_success_and_bootstrap_failure`, `cli_tests::sequential_ephemeral_cli_runs_advance_profile_owned_worker_generations`, `autospawn_tests::real_run_short_idle_ttl_terminalizes_spawned_daemon` | A later command finds a held lock / stale owner, or two daemons per profile; QA gate `alive_after` flips true. |
| (1,8,9) Session created by a one-shot is listed by a LATER persistent daemon with `run_state: idle`, the same `run_id`, an advanced `worker_generation`; the journal replays every streamed envelope identically (JSONL terminal fields stripped), followed only by session-state facts, contiguous `seq`. | NEW `one_shot_run_state_is_visible_to_a_later_persistent_daemon` | A shutdown that skips the journal flush / store close loses the run or its tail; consumers resuming after `seq` see gaps. |
| (9) A lingering daemon serves the next `haider run` itself (same PID, same generation) and `haider daemon stop --json` reports `stopped_cleanly` / `graceful` / `process_exited: true`. | NEW `one_shot_run_state_is_visible_to_a_later_persistent_daemon`, `persistent_daemon_cache_diagnostic_key_is_durable_across_restart`; existing `autospawn_tests::repeated_run_invocations_pay_one_cold_daemon_start_then_idle_exit`, `cli_tests::one_shot_never_shuts_down_a_prestarted_incumbent` | One cold start per invocation, or the operator stop door loses its receipt. |
| (8,9,10) JSONL surface for one fixed fake script is a golden: acceptance object (`head_seq: 1` on a fresh profile), 21 envelopes in cursor order, exactly one typed terminal (`success`) as the last line, stderr silent, LF framing, no CR. | NEW `one_shot_jsonl_stream_matches_the_normalized_golden`; existing `cli_tests::run_jsonl_announces_acceptance_before_lf_framed_envelopes`, `cli_tests::run_jsonl_replays_every_envelope_to_a_slow_pipe_consumer`, `cli_tests::run_exit_codes_are_table_driven`, `cli_tests::run_jsonl_bootstrap_failures_always_end_in_a_typed_error_record` | Folding Hello into the first RPC, shutdown-in-background, or a lazy seed that races the first turn drops/reorders/duplicates an envelope or adds a record. |
| (1) CAS objects written by a one-shot are durable after exit: the journal names `blake3:<64hex>`, the object exists under `profile/cas/<2hex>/<hex>`, and `FileCas::open(profile).get(ref)` returns the bytes. | NEW `one_shot_attachment_is_durable_in_the_profile_cas`; existing `haider-store::cas_tests::generic_put_batch_retains_one_trailing_full_fence`, `put_reader_publishes_mutated_source_bytes_under_their_actual_digest` | Coalescing the virgin-namespace publication into one F_FULLFSYNC that skips the object or directory entry leaves an unreadable address. |
| (1) A LATER daemon re-reads that CAS object when projecting history: the second turn's provider request still carries the file text; the session survives with an advanced generation and its replay names the artifact. | NEW `attachment_written_by_the_first_daemon_is_projected_by_the_next_daemon` | Lost namespace publication or a missing directory entry makes the next daemon fail attachment resolution (no `done`) or project the turn without the file. |
| (2) A PERSISTENT daemon's cache-diagnostic key is durable: `cache-diagnostic.key` exists (32 bytes, `0600`) once a turn ran, is byte-identical after `daemon stop` + restart, and `usage.request.cache.breakpoint_hashes.system` for the same prompt is identical across the restart. | NEW `persistent_daemon_cache_diagnostic_key_is_durable_across_restart`; existing `session_hub_private_tests::cache_diagnostic_key_is_persistent_exact_length_and_private`, `haider-core::actor::cache_diagnostic_*` (records never contain prompt/secret, bounded cost) | Regenerating the key per boot breaks cross-restart cache diagnostics; creating it lazily AFTER the first turn's diagnostics still fails the hash pin. |
| (2) A profile whose one-shot never wrote a durable key (or wrote one) does not break the later persistent daemon. | NEW `one_shot_run_state_is_visible_to_a_later_persistent_daemon` (fresh → one-shot → persistent lists/replays/serves), `custom_provider_delta_survives_daemon_restart_beside_the_builtin_catalog` (three successive boots) | A reader that requires the key at boot refuses to start on a one-shot-only profile. |
| (7) After a boot: `daemon-logs/haiderd-*.log` receives complete lines, `daemon.log` is published with identical content, and per-process history is bounded to `DAEMON_LOG_RETENTION` newest files (oldest pruned) by the time the one-shot CLI exits. | NEW `one_shot_boot_publishes_a_nonempty_daemon_log_and_bounds_log_history`; existing `haider-platform::spawn::lock_winner_publishes_the_stable_legacy_log_without_copying`, `per_process_log_history_is_count_bounded`, `diagnostic_log_tests::real_daemon_diagnostic_event_reaches_nonempty_per_process_log`, `concurrent_real_daemons_have_distinct_logs_with_intact_lines` | Moving log-dir maintenance off the spawn path without keeping it before TTL=0 exit lets `daemon-logs/` grow unbounded; skipping publication blanks `daemon.log` (CI-as-debugger reads it). |
| (5) No lockdown policy: the first command of a fresh daemon reports the default ceiling (`quota_used 0`, `quota_limit 1 GiB`, the fixed allowed-tool list), refuses nothing, and a quota change lands in `~/.haider/lockdown/quota.json`. | NEW `fresh_daemon_lockdown_status_reports_defaults_without_a_policy`; existing `haider-client::lockdown_tests::typed_status_and_quota_responses_do_not_parse_prose`, `feature_absence_makes_lockdown_helpers_absent` | Lazy reconciliation that never installs the manager returns the "unavailable before daemon startup" error on the first status/quota door. |
| (5) A pre-existing ledger + provider data: the first command sees the RECONCILED usage (real bytes, not the stale `used`), lowering the quota below it exits `76` and leaves the ledger untouched, raising it persists `limit` and reconciled `used`. | NEW `fresh_daemon_reconciles_seeded_lockdown_quota_before_its_first_command`; existing `lockdown_tests::quota_is_global_across_providers_and_reconciles_on_start`, `quota_cannot_be_lowered_below_reconciled_use`, `unchanged_startup_scan_does_not_rewrite_the_quota_ledger`, `restart_recovers_private_ledger_and_data_temporaries`, `turn_binding_survives_restart_and_rejects_provider_mismatch` | Reconciling lazily AFTER the first response lets `--set 10` succeed against 40 real bytes; a policy-gated skip that misreads "policy exists" reports `used: 0`. |
| (3) The built-in catalog a fresh profile exposes (`haider models --json`: 13 providers, families, endpoints, auth methods, seeded inventories, default models; sorted ids) is a golden and `provider list --json` names the same ids. | NEW `fresh_profile_models_catalog_matches_the_golden`; existing `provider_registry_tests::unchanged_json_registry_boot_does_not_rewrite_the_file`, `changed_json_registry_boot_still_replaces_the_file`, `all_built_in_provider_records_are_full_trust`, `builtin_without_cached_models_is_unknown_not_available_with_guesses`, `bedrock_and_vertex_model_details_get_effort_ladders_but_no_speeds` | A build-owned serialized catalog that drops/renames a provider, loses a seeded inventory row, or changes a family/endpoint changes the golden. |
| (3) A user-added provider (delta) survives daemon restart beside the complete built-in catalog with its discovered inventory, endpoint, family, trust, and default model. | NEW `custom_provider_delta_survives_daemon_restart_beside_the_builtin_catalog`; existing `cli_tests::configured_custom_model_reaches_chat_wire_verbatim_despite_catalog`, `provider_registry_tests::new_custom_provider_defaults_full_and_typed_setter_persists`, `provider_registry_removes_only_custom_profiles_and_clears_models`, `json_registry_loads_and_preserves_top_level_fallback_chain` | Rebuilding from the catalog alone, or not persisting the delta before the one-shot exit, drops `delta-proxy` (or its models) on the next boot. |
| (9) The first `loom.list` on a fresh profile returns exactly `scout` + `reviewer` (rev 1, seeded names/colours/glyphs); a restarted daemon returns the identical registry (no re-seed, no loss). | NEW `fresh_daemon_first_loom_list_returns_the_two_seeded_defaults_across_restart`; existing `loom_seed_tests::seeding_is_absent_only_and_never_clobbers_a_user_revision` | A lazy/batched seed that lands after the first read answers an empty registry; a re-seed on boot bumps revs and clobbers user edits. |
| (10) `status --json --no-spawn` on a virgin profile exits `69` and creates no daemon; the first spawned daemon reports `.daemon.version == CARGO_PKG_VERSION`, `ready: true`, `generation: 1`, `session_count: 0`, and the PID published in `lock.owner`. | NEW `fresh_profile_status_is_typed_without_a_daemon_and_reports_the_build_version_with_one`; existing `status_discovery_smoke_tests::built_status_json_honors_private_xdg_with_enabled_discovery`, `observe_cli_tests::no_daemon_no_spawn_paths_are_typed_69_and_do_not_start_a_daemon`, `version_tests::daemon_version_is_exact_and_has_no_profile_side_effect` | Handing the child a resolved-profile snapshot that disagrees with the parent breaks endpoint discovery; `Welcome.daemon_version` regressions surface here. |
| (10) An incompatible client protocol range receives the fatal typed `protocol_version_mismatch` refusal; a second Hello after the handshake is fatal; a silent peer is cut at the handshake deadline. | existing `daemond::lifecycle_tests::handshake_version_mismatch_returns_fatal_rejection`, `duplicate_hello_after_handshake_is_a_fatal_unexpected_frame`, `silent_peer_is_closed_at_the_handshake_deadline_and_frees_its_slot`, `haider-rpc::negotiation_tests::negotiation_rejects_disjoint_protocol_ranges` | Folding Hello into the first RPC must keep negotiation-before-dispatch; a dropped refusal turns skew into silent degradation. |
| (9) Stale endpoint / stale PID recovery after Ready, single-winner election, loser exit 75, launcher notified at Ready. | existing `autospawn_tests::stale_owner_socket_is_recovered_by_the_winning_daemon`, `two_simultaneous_launchers_elect_one_daemon_and_both_reach_ready`, `a_second_daemon_candidate_for_one_profile_exits_seventy_five`, `daemond::lifecycle_tests::abrupt_death_kill_9_leaves_recoverable_socket_and_next_start_serves`, `stale_pid_reuse_is_diagnostic_only_and_does_not_block_start`, `simultaneous_start_n_processes_has_one_winner_and_clean_losers`, `failed_listener_startup_publishes_failed_and_releases_profile_lock`, `ephemeral_liveness_tests::killed_spawning_client_reaps_ephemeral_daemon_and_runtime_files`, `killed_ready_spawning_client_reaps_ephemeral_daemon_and_runtime_files`, `killed_spawning_client_cannot_orphan_a_lingering_daemon` | Sweeping stale endpoints after Ready must not remove a live replacement; inline launcher notification must still follow a served Welcome. |
| (4) Fresh profile usage-history bootstrap: installation id stable across reopen/backfill, backfill version advances once, backfill idempotent, open quarter never finalized. | existing `haider-store::usage_ledger_tests::profile_installation_id_survives_reopen_and_backfill`, `journal_backfill_marks_its_day_header`; `usage_report_tests::session_folder_attributes_tokens_cost_duration_and_loc` | A schema-zero fast path that skips `installation_id`/backfill marker makes the next daemon's ledger reads `corrupt` (device mismatch) or re-backfill. |
| (4) The durable `usage` envelope of a one-shot turn (input/output, request cache diagnostic, scope) is journaled and replays identically to a later daemon. | NEW `one_shot_run_state_is_visible_to_a_later_persistent_daemon` (envelope-for-envelope replay), `one_shot_jsonl_stream_matches_the_normalized_golden` (usage line shape); existing `cli_tests::replay_is_a_read_only_exact_durable_projection` (`usage_matches`) | Moving the usage timer or dropping the dedicated runtime must not lose the correlated usage fact the reducer folds before the terminal. |
| (6) Ephemeral runtime profile still completes a turn end-to-end (tools, permissions, budgets, cancellation, timeout) on a one-shot. | existing `cli_tests::run_write_and_exec_permission_flags_journal_ordinary_allow`, `run_jsonl_cancelled_has_130_exit_and_terminal_envelope`, `run_jsonl_timeout_has_one_distinct_timeout_terminal`, `run_jsonl_unknown_pricing_has_one_distinct_budget_terminal_and_exits_77`, `kill9_after_provider_admission_exposes_typed_probe_recovery`, `staged_run_with_resident_daemon_has_two_steady_state_threads` | A 2-worker runtime that starves a blocking tool or the usage timer hangs a turn or drops the typed terminal. |
| (9) Idle TTL never retires a daemon with a non-terminal run; launcher death arms a bounded linger. | existing `autospawn_tests::idle_ttl_never_retires_a_daemon_with_a_nonterminal_run`, `daemon::lifecycle_tests::launcher_death_is_retained_as_typed_idle_shutdown_reason`, `launcher_death_can_arm_a_bounded_idle_linger`, `ephemeral_liveness_tests::second_client_holds_ephemeral_daemon_until_its_disconnect` | Shutdown-after-owned-work that ignores a second attached client or a queued run cancels durable work. |

## Needs a hook — pin after implementation

These behaviours are real but not observable through a public door on the
current tree without a product/test-support hook. None were turned into
`#[ignore]` tests.

1. **Usage-history sample of a one-shot turn reaches `usage.history_day`.**
   `initialize_usage_history` / `reconcile_usage_history` append only CLOSED
   15-minute slots (`address_start + 15 min <= now`), so a just-completed turn
   is not queryable until the wall clock crosses the quarter. Observed manually:
   a persistent daemon started after a one-shot folded the one-shot's turn into
   `usage/<date>.jsonl` (`t:"s"` row for `fake`/`fake-model`) once the slot
   closed. Hook needed: a test-only clock override (or "close the open slot"
   door) on the store's `now_ms`. Then pin: one-shot → advance clock → persistent
   daemon → `usage.history_day` contains one slot with input 2 / output 1.
2. **Lockdown tool refusal on the very first request of a fresh daemon.** The
   `lockdown.refused` payload for a provider with `trust: lockdown` needs a
   session bound to that provider whose scripted turn calls a denied tool.
   `HAIDER_TEST_FAKE_PROVIDER` scripts route every turn to the built-in `fake`
   provider (Full trust) and the in-process `RoutingFactory` resolves by name,
   so a lockdown-trust custom provider must be registered AND routed to a
   `FakeProvider` with `EmitToolCall{name:"process_exec"}`. Hook: none strictly
   required, but it is a ~150-line daemond fixture; pin after implementation as
   `first_request_of_a_fresh_daemon_refuses_a_denied_tool_under_lockdown`.
   Until then the quota door pins reconcile-before-first-command.
3. **Coalesced virgin-namespace F_FULLFSYNC (item 1) crash consistency.** The
   number of fsyncs is not contract-visible; durability after a clean exit IS
   pinned (CAS readback, cross-daemon projection). A kill -9 immediately after
   CAS publication needs a kill point analogous to
   `HAIDER_TEST_JOURNAL_KILL_AFTER` (`turnperf_support.py`) for the CAS/namespace
   publication. Pin after the hook lands: publish → SIGKILL at the point → next
   daemon reads the object or reports it absent, never a torn file.
4. **Ephemeral runtime profile (item 6): worker count / usage timer placement.**
   Not observable. A daemon-side steady-state thread-count sample at Ready
   (mirroring `staged_run_with_resident_daemon_has_two_steady_state_threads` for
   the CLI) needs a "daemon is at steady state" signal the test can wait on.
5. **Hello folded into the first RPC (item 10).** Daemon-side negotiation pins
   exist (above). A client-side pin that `haider run` still emits exactly one
   `Hello` before its first request needs a wire capture (`HAIDER_DAEMON_TRACE`
   frame log or a client-side frame counter) exposed to tests.
6. **Registry bootstrap single-read / no SQLite page release before the first
   TTL=0 turn (item 9).** Internal I/O counts. The observable is the models golden
   plus `unchanged_json_registry_boot_does_not_rewrite_the_file`; an I/O-count
   hook (tracing target `haider.recovery`-style counters for registry reads and
   `PRAGMA` calls) would let a pin assert "one read, zero releases".

## Run evidence

Environment: `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
HAIDER_TEST_SIBLINGS_PREBUILT=1 CARGO_BUILD_JOBS=2`, macOS, `cargo build -p
haider-daemond --bin haiderd -p haider-cli --bin haider` first.

Verbatim (2026-09-01, current tree, no product-code changes):

- `cargo clippy -p haider-cli --tests -- -D warnings` -> rc=0;
  `cargo clippy -p haider-daemond --tests -- -D warnings` -> rc=0.
- `cargo test -p haider-cli` -> 13 binaries, all `0 failed`; the new file:
  `test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.03s`
  (`cli_tests`: `120 passed`, `autospawn_tests`: `10 passed`, `status_discovery_smoke_tests`: `4 passed`).
- `cargo test -p haider-daemond` -> 18 binaries, all `0 failed`;
  `core_loop_e2e_tests`: `test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.02s`
  (the two new pins alone: `test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 18 filtered out; finished in 0.23s`).
- `cargo run -q -p xtask -- test-count --update` -> `test-count: baseline updated to 4363`
  (`test-baseline.txt` 4351 -> 4363, +12 = the twelve tests above).

## Hygiene

- Every test stops the daemon it spawned (`Profile::stop_daemon_cleanly` or the
  `Profile` drop guard: `haider daemon stop --json --timeout 5s`, then a targeted
  signal to the PID in `lock.owner`). Nothing kills `haiderd` broadly.
- No sleeps as synchronization: every wait is a bounded poll
  (`wait_for_daemon_gone`, `bounded_output`, `CatalogServer` accept loop).
- No `#[ignore]`, no test-level platform gating; the only `cfg(unix)` is the
  profile-lock probe and the `0600`/`0700` mode assertions.
