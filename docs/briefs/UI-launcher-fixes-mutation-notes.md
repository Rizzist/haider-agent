# UI launcher-fixes mutation notes (centered block + summary counts)

Each mutation below was applied to production code, its separate observer was
run to an assertion-level RUNTIME failure (`running 1 test` verified on every
run — no vacuous filters), and the mutation was reverted against the committed
tree. No compile-only failure is claimed as evidence. Fixtures are
non-degenerate: the centering pins run at THREE widths (130/80/70 — above the
cap twice, at it once), the summary fixtures carry BOTH truth flags (Estimated
and Exact) with distinct token figures, and the checkin fixture replays a real
attach → events → checkin round-trip through the live driver's reply path.

| # | Production mutation (applied → reverted) | Runtime observer (`launcher_fixes_tests`) | Observed RUNTIME failure |
|---|---|---|---|
| M1 | Forced `center_pad` to `0` in `render_launcher` (the old left anchor). | `launcher_content_block_is_centered` | Head-row column pin failed: `left: 1` vs `right: 31` at 130 cols. |
| M2 | Kept the AttachSession hit rects on the UNSHIFTED left edge (`x: content_area.x - center_pad`) while the paint stayed centered. | `launcher_content_block_is_centered` | `hit rect moved WITH the centered paint` failed: `left: 0` vs `right: 30`. |
| M3 | Collapsed `SessionState::turns` to the bare `turns_offset + user_row_count()` (summary branch deleted). | `roster_shows_summary_counts_without_attach` | `row containing "12 turns" not rendered` — the hydrated count vanished. |
| M4 | Relaxed `SessionState::summary_is_fresher` from `head_seq > applied` to `>=`. | `checkin_values_beat_stale_summaries` | Post-checkin pin failed with the row re-dressed as `… 99 turns · ~999k tok …` where the replayed truth was `1 turn`. |
| M5 | Dropped the Estimated marker from `row_tokens` (returned `false` unconditionally). | `roster_shows_summary_counts_without_attach` | `estimated footprint wears the honest ~ prefix` failed with `… 12 turns · 48k tok …` (tilde gone). |
| M6 | Deleted `note_summary_counts`' both-fields-absent guard (empty summaries stored). | `older_daemon_without_fields_degrades_honestly` | `nothing stored — nothing to fabricate from` failed — a field-less summary minted a `SummaryCounts` (and the later field-less list would wipe real values to zero). |
| M7 | Removed the `model.note_summary_counts(&summary)` call from the driver's `Listed` arm (wiring cut). | `roster_shows_summary_counts_without_attach` | `row containing "12 turns" not rendered` — summaries decoded but never reached the roster. |

Adjacent structural pins: the ≤-cap rung of `launcher_content_block_is_centered`
(70 cols → column 1, rect.x 0) holds the degradation law — a mutation that pads
narrow frames fails it; `tui4_owner_wave_tests::
the_launcher_column_is_capped_and_centered_at_a_wide_frame` holds the cap and
one-shared-column laws at 165 cols with the centered edge pinned exactly (48).

## Honest flips carried by this wave

* `tui4_owner_wave_tests::the_launcher_column_is_capped_and_left_anchored_at_a_wide_frame`
  → renamed `…_capped_and_centered_at_a_wide_frame`: the ui-themes wave pinned
  the left anchor (`edges[0] == 1`); the owner reversed it, so the pin now
  holds the centered edge (`edges[0] == 48` at 165 cols) with the cap and
  shared-column assertions unchanged.

## Wire coordination noted honestly

The additive `SessionSummary` fields (`turn_count`,
`latest_context_footprint`) were NOT on origin/main when this wave shipped —
the parallel daemon lane owns their population. This wave defines them
defensively (`serde(default)` + `skip_serializing_if`, absence ≡ older
daemon) in haider-rpc and fills `None` in `session_hub::rpc::session_list`
with a lane-handoff comment. The exact/estimated flag rides INSIDE the
footprint (`ContextFootprint::truth`), mirroring
`SessionReadResult::latest_context_footprint` — if the lane lands different
field names, `AppModel::note_summary_counts` is the single consumption seam
to re-point.
