# W-A — long-lived background shell tasks + session completion messages — notes

Implementer: Fable 5. Branch `wa-background-tasks` from main @ v0.0.77.
Owner contract: "everything should be long lived, and when done, like
subagents, show session msg (like claude code). implement now."

## Shape (locked decisions 1–8, as built)

**One tool, one flag (decision 1).** `process_exec` gains optional
`background: bool` + `name: string` (defaulted from the command's first
token, `haider-tools/src/tasks.rs::default_task_name`). Foreground is
untouched — the background branch in the dispatcher's `ProcessExec` arm
returns before the foreground path is entered. The background call
completes IMMEDIATELY with the typed `{task_id, name, state: "running"}`
result (LT1). The `!` composer escape stays foreground-only structurally:
it rides `LiveCommand::ShellExec` → `perform_shell_exec` →
`process_exec_user`, a path with no background parameter (pinned by the
existing `live_bang_routes_one_exact_command_to_shell_exec_and_never_a_turn`
plus the new `foreground_process_exec_is_unchanged_and_journals_no_task_facts`).

**Journal is the truth (decision 2).** New additive fact union
`haider_protocol::task::TaskEventPayload` (`task_started` /
`task_completed`) — the S3 `AgentEventPayload` pattern: OUTSIDE
`EventPayload`, riding `RawEnvelope`, ZERO rpc-frame changes (golden
`golden_additive_task_facts_and_unknown_kind_tolerance` pins shape + raw
tolerance; no existing fixture changed). The hub-owned `TaskRegistry`
(`haider-daemon/src/tasks.rs`) is a projection: per-session, rebuilt
lazily ONCE per daemon life by `TaskFacade::adopt_session` (supervisor
start + every task-tool touch). Adoption reaps orphans:
started-without-completed → injectable pid-liveness probe (`PidLiveness`
seam, LT6) → TERM → grace → KILL on the stale pgid → honest
`Failed{reason}` completion fact with a deterministic event id
(`task-completed-{task}` — restart-idempotent because the scan sees any
committed completion first).

**Hardened spawn (decision 2 / gate52).** `EffectBroker::
process_exec_background` (`haider-tools/src/tasks.rs`) reuses the exact
foreground confinement — `PreparedProcessExec` anchored-cwd re-walk
(TOCTOU defense), `env_clear`, own process group — and ADDS the gate52 fd
close-sweep (`close_inherited_descriptors`, the daemon auto-spawn
lesson): a background child outlives the turn, so a leaked pipe end
outlives the caller's expectations with it. stdin is null. The effect
journals all four phases and terminalizes Ok AT THE SPAWN BOUNDARY (the
`spawn_subagent` precedent) — the child is never entered into the
broker's foreground `ProcessRegistry`, so broker close (turn end, esc)
cannot cancel it. Outliving the turn is the feature (decision 6; pinned
in `background_spawn_returns_immediately_and_survives_broker_close` and
the delete-fence test's dispatcher-close assert).

**Bounded output (decision 3, LT2).** `TaskOutputBuffer`: retained head
(cap `TASK_OUTPUT_RETAIN_BYTES` = 512 KiB), rolling tail
(`TASK_TAIL_BYTES` = 4 KiB), total byte counter. Past the cap output is
DROPPED — never buffered, never fatal: the task keeps running and
`truncated()` marks the drop honestly. On completion the retained bytes
become one CAS artifact referenced by the completed fact.

**Completion is a session message (decision 4).** The detached completion
pipeline (`TaskFacade::complete_task`): artifact → delivery → fact →
registry settle.
- IDLE session → `delivery: delivered_queued`; the completed fact
  journals `render.prompt = Verbatim`. The prompt compiler
  (`haider-core/src/prompt_history.rs::render_journal`) renders task
  facts as bounded user-role notices BEFORE the run-terminal gate — a
  task outlives a CANCELLED spawning run by design, and that gate would
  otherwise silence it (LT3; the cancelled-run case is pinned in
  `task_facts_reach_the_next_turn_prompt_and_omit_is_the_off_switch`).
- ACTIVE run → the SAME notice text (`haider_core::task_event_notice` is
  the one authority, so steer and prompt can never diverge) is delivered
  as a durable STEER: `accept_internal_turn` (SteerPending) +
  `submit_internal_nudge` — exactly the S1 `message_subagent` seam. The
  durable steer IS the delivery (a restarted turn recompiles with it),
  so a failed live wake never demotes to queued. The fact then journals
  `render.prompt = Omit` — exactly ONE prompt copy either way (LT4).
- Steerable = nonterminal, not Cancelling, and NOT Queued: a queued run
  has no live harness, and its prompt compiles AFTER the fact lands, so
  the Verbatim fact already reaches it. A disposition race (run ended
  between scan and accept) cancels the stray admission — a completion
  must never START provider work.

**Tools (decision 5, LT5).** `task_output {task_id, cursor?}` — no
broker (the request_input/message_subagent actor-owned pattern): tail
preview without a cursor, 8 KiB retained-range pages with one; unknown
task → typed `unknown_task` result. `task_kill {task_id}` — IS an effect
under the EXISTING process ceiling (`EffectClass::ProcessExec`, Ask
default like `process_exec`; the `allow_exec` session override lifts
both together, so delegated children kill freely): intent/outcome
journal around the supervised TERM → grace → KILL pgid ladder, and Ok
means the group provably settled. No `/tasks` listing tool this wave
(decision 5/7 — the rows are ambient; revisit).

**Lifecycle fences (decision 6, LT7).** Cap `TASK_CONCURRENCY_CAP` = 8
running per session — typed `task_cap_reached` refusal, never an error,
never sticky. Session delete: `fence_background_tasks` runs after the
actor provably stops and before the durable delete — registry ladders
kill this-life tasks (their pipelines find the projection gone and
record nothing into the deleted journal); journal-only prior-life
orphans get detached reaps. Daemon shutdown: `shutdown_background_tasks`
BEFORE the drain flag so completion facts can still journal; anything
unsettled is reaped by next-start adoption. Turn cancellation (esc)
touches nothing — pinned twice (broker-close and dispatcher-close
survival asserts).

**TUI (decision 7).** `haider-tui/src/taskrows.rs`: session-scoped
`TaskPanel` traveling WHOLE with the session at checkout/checkin like
`hook_facts` (tasks are session-scoped by runtime law, never split per
branch; only the ambient NOTE lands on the stamped branch's timeline).
Both raw-routing twins try `route_task_event` after `route_agent_event`
before counting a payload unknown. Completion row = transcript note in
the S3 voice (`└ task {name} — exit 0 · 42s — {tail line}`), which gives
plain mode its parity line for free (`plain::render_plain` prints
notes). Running tasks render as ONE band line above the composer
(`⚙ N background task(s) — name 42s · …`, three names then `+N more`),
gold sigil + dim ink beside the waiting line (same shed priority, one
shared breathing row); elapsed ticks on the S4 journal clock and
`animated()` holds the tick gate open while any task runs.

**Headless (decision 8).** `haider run` still exits when the TURN
completes. The reducer tracks task facts session-wide; the run summary
names still-running tasks on stderr, and the `haider.run.v1` object
gains the additive `background_tasks_running` field (byte goldens +
key-count pins regenerated honestly, 11 → 12 keys). The tasks stay
daemon-owned and end when the session closes (decision 6), documented in
the stderr note itself.

## Notable calls + residuals

- The LT4 test taught a harness law worth keeping: a SECOND worker lease
  on a session with a live turn SUPERSEDES the supervisor's lease and
  fences the harness's appends — production dispatchers share the turn's
  lease, so the steer test spawns through the facade with its own broker.
- Store law respected, not changed: worker-lease appends (broker effect
  phases) require an accepted, nonterminal run; `SessionHub::append`
  (facts/projections) does not — which is exactly what lets a completion
  fact land after its spawning run ended.
- `cargo clippy -- -D warnings` is NOT clean at baseline (pre-existing
  `haider-rpc` large_enum_variant errors + `haider-daemon` accounts.rs
  lints); touched crates were linted with `--no-deps` and all NEW code is
  clean under `-D warnings`.
- `flagless_run_without_an_active_account_exits_65_with_remedy` flaked
  once under full-suite parallelism (daemon-spawn contention); green in
  isolation and on the full-suite rerun — unrelated to this wave.
- Registry snapshot on tool-inventory/observe surfaces: NOT exposed
  (decision 5's "if trivially exposible" — it is not; the completion and
  started facts plus `task_output` carry the state).
- Real-process fixtures are short-lived `/bin/sh` commands throughout;
  test kill ladders run with 200–300 ms grace and every fixture is
  reaped (the orphan test reaps its own zombie explicitly).

## Law tests (all runtime, by name)

- haider-protocol: `golden_additive_task_facts_and_unknown_kind_tolerance`.
- haider-tools (`background_tasks_tests.rs`, 7):
  `background_spawn_returns_immediately_and_survives_broker_close` (LT1),
  `background_output_is_bounded_with_honest_truncation` (LT2),
  `task_kill_ladder_kills_the_whole_process_group` (LT5),
  `natural_exit_sweeps_lingering_group_members`,
  `orphan_reap_honors_the_liveness_seam_and_kills_stale_groups` (LT6),
  `task_names_default_from_the_command_and_validate`,
  `background_spawn_respects_ask_policy`.
- haider-core (`prompt_history_tests.rs`):
  `task_facts_reach_the_next_turn_prompt_and_omit_is_the_off_switch` (LT3 compiler half).
- haider-daemon (`tasks_runtime_tests.rs`, 7):
  `background_dispatch_is_immediate_and_kill_is_brokered_end_to_end` (LT1+LT5),
  `foreground_process_exec_is_unchanged_and_journals_no_task_facts` (LT1 regression),
  `idle_completion_fact_is_bounded_with_cas_artifact_and_prompt_notice` (LT2+LT3),
  `active_run_completion_steers_mid_turn_with_exactly_one_durable_nudge`
  (LT4 — the durable nudge COUNT is asserted, the W6 vacuous-pin lesson),
  `ninth_task_is_refused_and_shutdown_fence_kills_running_groups` (LT7),
  `restart_adoption_reaps_stale_pgids_through_the_liveness_seam` (LT6),
  `session_delete_fence_kills_the_running_group` (LT7 + esc/turn-end law).
- haider-tui (`wa_task_rows_tests.rs`, 4):
  `task_facts_paint_started_and_completion_notes_and_never_count_unknown`,
  `running_band_ticks_on_the_journal_clock_and_sheds_at_completion`,
  `plain_mode_prints_the_same_task_lines`,
  `completion_without_started_still_lands_a_terminal_row`.
- haider-cli: `print_and_json_outputs_pin_bytes_schema_and_nulls` +
  `run_json_reports_attachments_additively` (v1 goldens regenerated for
  the additive field).

Mutation campaign: `WA-background-tasks-mutation-notes.md` — 8 EXECUTED
kills (immediate return, steer delivery, orphan reaping, output
bounding, kill fence, cap refusal, single-prompt-copy, TUI note
routing), each commit-before-mutation, single-anchor, `running 1 test`
observed, runtime failure recorded, reverted, green.
