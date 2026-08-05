# S4 subagent-rows mutation notes

Every mutation below was EXECUTED against
`cargo test -p haider-tui --test s4_subagent_rows_tests` (mutation applied
to production code, tests run, RUNTIME failure observed, mutation
reverted, suite re-run green). "Expected RUNTIME failure" is the observed
assertion, never a compile error.

| # | Production mutation (EXECUTED) | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| M1 | `render_subtree`: drop the right-align pad — anchor the meta one space after the activity text. | `live_chip_elapsed_ticks_on_the_render_clock` | FAILED — the pad-gap assertion: no `"  "` separates content from the meta, the row reads left-anchored. |
| M2 | `chip_row_meta`: truncate the full meta to the budget instead of dropping whole segments. | `width_degradation_drops_tokens_first_then_elapsed_whole` | FAILED — the 76-col frame carries a cut token fragment (`… · ↓ 266k t`) where elapsed-only is law; the 62-col frame carries a cut elapsed. |
| M3 | `ChipModel::elapsed_ms`: terminal chips keep the live formula (`clock − spawn`). | `terminal_chip_elapsed_freezes_at_the_terminal_envelope`, `later_envelopes_never_move_a_frozen_final`, `chip_clock_is_monotone_and_stops_at_terminal` | FAILED ×3 — the frozen figure follows the render clock (`1h …` leaks into a Done chip's row). |
| M4 | `chip_row_tokens`: join positionally (`find_map` over the roster) instead of exact-matching the chip's own `child_session`. | `tokens_join_by_the_chips_own_child_session_id` | FAILED — chip A wears chip B's `1.2k` figure (newest row first): the wrong-child law caught it. |
| M5 | `render_subtree`: render `↓ 0 tokens` when the truth chain says `None` (`unwrap_or(0)`). | `unknown_tokens_render_no_token_segment` (+ `live_chip_elapsed_ticks_on_the_render_clock`) | FAILED ×2 — a token segment appears with every source empty; the fabricated `0` also displaces the right-end elapsed assertion. |
| M6 | `ChipModel::note_event_at`: drop the terminal-freeze gate (keep advancing after Done). | `later_envelopes_never_move_a_frozen_final`, `chip_clock_is_monotone_and_stops_at_terminal` | FAILED ×2 — the post-Done report envelope stretches the final by five minutes of paperwork. |
| M7 | `AppModel::animated`: drop the `tree_live_count > 0` arm (pulse set only). | `a_live_chip_keeps_the_anim_clock_running` | FAILED — a streaming parent with an idle live child reports `animated() == false`: the elapsed figure would never tick. |
