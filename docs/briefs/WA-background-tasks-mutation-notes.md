# W-A background tasks — mutation notes (8 EXECUTED kills)

Discipline: the tree was COMMITTED before every mutation (campaign base
`2772acb`); each kill is one single-anchor production mutation, observed
by running exactly its one observer test (`running 1 test` confirmed in
every run), with the RUNTIME failure recorded verbatim, then reverted via
`git checkout --` and re-run green. No mutation touched test code.

| # | Area (brief list) | Production mutation (single anchor) | Runtime observer (`running 1 test`) | Recorded RUNTIME failure | Reverted → green |
|---|---|---|---|---|---|
| K1 | Immediate return (LT1) | `haider-daemon/src/tasks.rs::spawn_background`: hoist `let status = supervision.await;` ABOVE `tokio::spawn` — the dispatch waits for the child like a foreground call | `tasks_runtime_tests::background_dispatch_is_immediate_and_kill_is_brokered_end_to_end` | panic `background dispatch must return immediately` (tests_runtime_tests.rs:321) after **30.09 s** — the `sleep 30` child was awaited | ok, 0.38 s |
| K2 | Steer delivery (LT4) | `haider-daemon/src/tasks.rs::steer_completion`: `return Ok(false);` as the first statement — no durable steer, no wake | `tasks_runtime_tests::active_run_completion_steers_mid_turn_with_exactly_one_durable_nudge` | panic at :710 `assertion left == right` — `completed.delivery` is `DeliveredQueued`, not `DeliveredSteer` (0.15 s) | ok, 0.12 s |
| K3 | Orphan reaping (LT6) | `haider-daemon/src/tasks.rs::adopt_session_with_probe`: `let reap = reap_orphan_group(…).await;` → `let reap = OrphanReap::AlreadyDead;` — the liveness seam is never consulted, the stale pgid never signalled | `tasks_runtime_tests::restart_adoption_reaps_stale_pgids_through_the_liveness_seam` | panic at :938 — probe ledger empty: `the seam judges the live pid` (0.05 s) | ok, 0.26 s |
| K4 | Output bounding (LT2) | `haider-tools/src/tasks.rs::TaskOutputBuffer::append`: `let room = self.retain_cap.saturating_sub(self.retained.len());` → `let room = bytes.len();` — the retained head grows without a cap | `background_tasks_tests::background_output_is_bounded_with_honest_truncation` | panic at :169 `head retained to the cap` — `left: 8200, right: 1024` (0.02 s) | ok, 0.03 s |
| K5 | Kill fence (LT7) | `haider-daemon/src/session_hub/mod.rs::delete_fenced_session`: `self.fence_background_tasks(session_id).await;` → no-op — session delete leaves the pgid running | `tasks_runtime_tests::session_delete_fence_kills_the_running_group` | panic at :281 `process group dies: Elapsed(())` — the 10 s death wait timed out with the group still alive (10.06 s) | ok, 0.38 s |
| K6 | Cap refusal (LT7) | `haider-daemon/src/tasks.rs::spawn_background`: cap check `>=` → `>` — the ninth task spawns | `tasks_runtime_tests::ninth_task_is_refused_and_shutdown_fence_kills_running_groups` | panic at :821 `assertion left == right` — `left: Null, right: "refused"` (the ninth dispatch returned a running receipt, 0.08 s) | ok, 0.50 s |
| K7 | Single prompt copy (LT3/LT4 boundary) | `haider-daemon/src/tasks.rs::complete_task`: steer arm `PromptRender::Omit` → `PromptRender::Verbatim` — a steer-delivered completion journals a SECOND prompt copy | `tasks_runtime_tests::active_run_completion_steers_mid_turn_with_exactly_one_durable_nudge` | panic at :711 `the steer user message owns the ONE prompt copy` (0.09 s) | ok, 0.15 s |
| K8 | Completion row routing (decision 7 / LT3 projection half) | `haider-tui/src/session.rs::route_task_event`: `BranchScope::Active => projection.push_note(note)` → `drop(note)` — facts consumed but never painted | `wa_task_rows_tests::task_facts_paint_started_and_completion_notes_and_never_count_unknown` | panic at :138 `assertion left == right` — `left: []`, expected the started + completion note rows (0.00 s) | ok, 0.00 s |

Notes on fixture hygiene: K1/K5/K6's mutated runs briefly orphan
`sleep 30` fixtures (the failing test aborts before its own cleanup);
they expire on their own and the reverted green runs reap normally.

Coverage against the brief's mandatory list: immediate return (K1),
steer delivery (K2), orphan reaping (K3), output bounding (K4), kill
fence (K5), cap refusal (K6) — plus the single-prompt-copy law (K7) and
the TUI transcript-row seam (K8).
