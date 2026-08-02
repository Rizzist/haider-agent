# B2b-m3 mutation notes

Each mutation below was EXECUTED against the b2b-m3-tree branch on
2026-08-03: applied to production code, observed failing the named test at
RUNTIME (assertion or panic — never a compile error), then reverted. The
fork-coordinate fixture deliberately commits TWO distinct `(node, seq)`
pairs per branch (`node-1`/2 vs `node-2`/4, and `enode-*` on the fork) so
coordinate-substitution mutations change observable output — the
degenerate single-node fixture class this run has already killed three of
would have let mutation 7 pass silently.

## Milestone 1 — polish

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 1 | Restore the pre-m3 generic `(_, "oauth")` stub for openai/anthropic in the `/login` match (`app.rs`), flashing "lands after v0.0.12" instead of routing through `Hit::AccountAdd`. | `b2b_m3_polish_tests::slash_login_openai_and_anthropic_oauth_mirror_the_buttons` | Screen never reaches /accounts, no card, no `OAuthAddStart` — the mirror assertion fails on the stale flash. |
| 2 | Restore the anthropic-only slot-0 table in `commands::login_args`. | `b2b_m3_polish_tests::palette_login_slots_name_the_real_roster` | The four-provider roster equality fails (`["anthropic"]` ≠ `["anthropic","openai","gemini","kimi"]`). |
| 3 | Delete the `OAuthFlowStatusWire::WaitingDevice` arm from `LiveDriver`'s `OAuthFlowStatus` match so the status falls through the tolerant `_ => {}` arm (`live.rs`). | `b2b_m3_polish_tests::waiting_device_maps_to_device_honest_copy` | The card phase stays `WaitingBrowser` and the rendered frame still shows the loopback "your browser opened…" line — the `WaitingDevice` destructure panics. |

## Milestone 2 — the tree screen

| # | Production mutation | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| 4 | Skip the `push_forks_at` call inside `tree_rows`'s node walk (`app.rs`) so fork markers trail at the end instead of under their exact fork node. | `b2b_m3_tree_tests::tree_opens_at_the_root_and_nests_fork_markers_under_the_exact_fork_node` | Row 2 is `node-2`'s row, not the `⑂ experiment` marker — the immediate-under-the-fork-node assertion fails. |
| 5 | Make the tree's root-esc arm call `back_to_launcher()` instead of returning to the session (`app.rs`). | `b2b_m3_tree_tests::drill_breadcrumb_and_esc_walk_parent_then_close_session_scoped` | The final screen is `Launcher` with `active_session == None` — the session-scoped esc law assertion fails. |
| 6 | Make the `Hit::TreeRow` arm ignore the carried row value ("bounds check only": select `rows.len()-1` for any hit) (`app.rs`). | `b2b_m3_tree_tests::selection_clamps_and_a_stale_hit_on_a_replaced_row_cannot_activate` | The ghost-row hit moves the selection from 2 to 3 — the replaced-row refusal assertion fails. |
| 7 | Make `tree_fork_selected` substitute `branch_state.fork_point()` (the tracker's LAST committed node) for the selected row's coordinates (`app.rs`). | `b2b_m3_tree_tests::f_issues_the_selected_rows_exact_coordinates_not_the_trackers` | The issued `AppRequest::BranchCreate` carries `node-2`/seq 4 instead of the selected `node-1`/seq 2 — the request equality fails. |
| 8 | Resolve the jump anchor to the entry's LOGICAL line index instead of `row_of_line[line]` (skip the wrapped-row prefix sums) (`render.rs`). | `b2b_m3_tree_tests::enter_on_a_node_row_lands_the_render_resolved_jump` | The wrapped history above T4 shifts the landing — `T4Q` renders below the transcript top, the top-alignment assertion fails. |
| 9 | Drop the near-tail clamp: `target_top = row` unclamped before `max_scroll - target_top` (`render.rs`). | `b2b_m3_tree_tests::a_near_tail_target_clamps_honestly` | `attempt to subtract with overflow` panic at the u16 subtraction (`render.rs:1890`) — the tail target's row exceeds `max_scroll`. |
