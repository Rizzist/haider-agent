# B2b m1+m2 TUI-branches mutation notes

Every mutation below was EXECUTED against the suite on 2026-08-01 (apply →
observe the runtime failure → revert). "Expected RUNTIME failure" means the
named test fails by assertion; a compile-only failure is not the claimed
evidence. All observers live in `haider-tui/tests/b2b_branch_state_tests.rs`
and `haider-tui/tests/b2b_branch_command_tests.rs`.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| `BranchState::scope_of` routes branch-stamped content `Active` instead of `ParkedNamed` (sibling traffic paints the displayed view). | `branch_created_installs_a_warm_view_and_stamped_content_stays_out_of_main` (+6 sibling-isolation tests) | Main's transcript gains the fork's row; the warm view is empty after the switch; chip/compaction/footprint isolation assertions all fail (7 failures observed). |
| Drop the session-cursor transplant from `BranchState::switch`. | `interleaved_branch_seqs_advance_one_cursor_and_switching_never_rewinds`, `background_slots_materialize_branch_views_through_the_same_router` | After a switch the stale parked cursor resurrects: a redelivered seq re-applies (the fork's row doubles) instead of `Duplicate`, and the background slot's cursor is not continuous (2 failures observed). |
| `AppModel::checkin` resets `branch_state` instead of writing it back to the session slot. | `branch_state_survives_session_a_to_b_to_a_checkout`, `inactive_branch_chips_stay_them…counts_them` | Session A returns from the A→B→A round trip on main with no registry; the detached slot's launcher aggregate loses its parked live child (2 failures observed). |
| Remove the `is_aggregate` type-first arm from `scope_of` (route aggregates by branch). | `aggregate_session_state_routes_session_global_off_a_branch_stamped_stream` | The branch-stamped `SessionState::Idle{interrupted}` vanishes into the fork's view: main's idle(i) marker never sets. |
| Run the `note_admitted` command-state hook only under `Admission::Apply` (skip the `render.ui == false` half). | `render_ui_false_never_mutates_display_but_records_command_coordinates` | The ui-false `NodeCommitted` is invisible to the tracker: `fork_point()` is `None`, so `/branch new` has no honest coordinates. |
| `LiveDriver::handle_request`'s `SubmitText` arm re-reads `model.branch_state.active()` instead of the request's captured branch. | `a_submit_queued_before_a_later_switch_still_carries_its_captured_branch` | The submit issued on the fork and drained after a switch to main goes out with `branch: None` — the queued turn was retargeted. |
| Encode `TurnSubmitWithBranch { branch_id: None }` for a main submit instead of the legacy `TurnSubmit` variant. | `main_branch_wire_bytes_stay_historical_and_branches_ride_the_decode_forms` | The variant pin fails. FINDING: the first run of this mutation passed the whole suite — serde skips a `None` `branch_id`, so the two variants are byte-identical on the wire and a bytes-only assertion cannot see the encode-selection law drift; the test now pins the VARIANT as well as the bytes. |
| Drop the `daemon_serves(FEATURE_BRANCH_CREATE_V1)` gate from `branch_command`. | `slash_branch_is_session_only_and_feature_gated` | The feature-ungated live daemon opens a picker card instead of the honest stale-daemon notice (fabrication the daemon can never resolve). |
| Drop the `BRANCH_CARD_PREFIX` intercept from `submit_menu_answer` (route the picker's answer through the outbox). | `picker_enter_and_digits_switch_and_close_the_card_locally` | The card never closes, the switch never happens, and a ghost answer sits in the outbox for a menu no daemon opened. |
| `branch_new` sends `fork_seq: 0` instead of the tracker's recorded sequence. | `slash_branch_new_issues_exact_captured_coordinates` | The `AppRequest::BranchCreate` equality on exact `{session, source_branch, fork_node_id, fork_seq, name}` fails. |
| `SessionState::live()` counts only the displayed chips (drop `parked_live`). | `inactive_branch_chips_stay_there_and_the_launcher_aggregate_counts_them` | The detached slot with a live child on an inactive branch reports `live() == 0` and `busy() == false` — the launcher row goes cold while a hidden child runs. |
| `branch_card` marks every row `○` (drop the active lookup). | `the_picker_lists_main_and_named_branches_with_the_active_marked`, `slash_branch_new_is_demo_honest_and_the_demo_picker_shows_main` | The `●` marker never follows the active branch in either mode (2 failures observed). |
| `render_status_bar` hardcodes the branch segment back to `" · main"`. | `the_status_bar_names_the_active_branch` | The active fork's name never reaches the status bar. |
| The driver's `BranchForked` arm activates the CURRENTLY ATTACHED session instead of the receipt's originating session. | `a_fork_receipt_activates_only_the_originating_session` | A background session's fork receipt switches the attached session's displayed branch (cross-session retargeting). |

Structural notes (not mutations):

- The fabricate-a-branch-locally mutation ("receipt installs the branch")
  is unrepresentable from the driver: `BranchState` exposes no public
  install API — only `note_admitted` (fed by admitted journal envelopes)
  writes the registry. The observer
  `no_live_branch_before_daemon_truth_and_the_journal_fact_activates_once`
  still pins the behavioral half (receipt → nothing installed, journal
  fact → one install + one activation, replays idempotent).
- `LiveCommand::Cancel`'s captured branch is deliberately client-side
  only: the wire `turn.cancel` pins the run by `run_id`, which acceptance
  already branch-pinned (`main_branch_wire_bytes…` asserts the cancel
  bytes carry no `branch_id` in both cases).
