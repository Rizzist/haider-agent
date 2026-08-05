# T2 — talk UX mutation campaign (EXECUTED)

House rules: every mutant below was APPLIED to the source, the targeted
suite RUN, the failure OBSERVED, and the mutant REVERTED; the full T2
suites were re-run green after the final revert. No mutant survived.

| # | Mutant (file · exact change) | Law it probes | Result |
|---|---|---|---|
| M1 | `talk.rs` `WaveRing::push`: `pop_front`+`push_back` → `pop_back`+`push_front` (newest enters at the LEFT) | ring geometry — newest-right, history flows left | KILLED — `ring_is_fixed_width_with_newest_at_the_right_edge`: newest landed at index 0, shifted-history assert broke |
| M2 | `talk.rs` `WaveRing::push`: swap `WAVE_ATTACK`/`WAVE_DECAY` in the rate pick | asymmetric smoothing (attack 0.5 / decay 0.13) | KILLED — `attack_rises_at_half_per_sample`: one loud sample from silence landed at 0.13, not 0.5 |
| M3 | `talk.rs` `wave_glyph_index`: drop the `.sqrt()` (linear mapping) | perceptual sqrt glyph mapping | KILLED — `glyph_mapping_is_sqrt_and_total_over_the_unit_interval`: 0.25 mapped to step 2 instead of 4 |
| M4 | `app.rs` `talk_cancel`: insert `realize_talk_ghost()` before `settle()` (Esc keeps the words) | Esc = DISCARD, nothing lands anywhere | KILLED — `esc_cancels_and_discards_everything`: the discarded ghost appeared in the composer |
| M5 | `app.rs` `talk_key` Char arm: return `true` (swallow the typed char after the commit) | typing commits AND keeps editing (the char flows the normal path) | KILLED — `typing_commits_the_ghost_and_keeps_editing`: composer ended `"hello world "` without the typed `x` |
| M6 | `app.rs` `handle_talk::Finished`: delete the generation + phase gate (accept any `Finished`) | staleness — a settled generation's late result is dead on arrival | KILLED — `a_late_finished_after_cancel_is_dropped_whole`: the canceled session's "sneaky late transcript" landed in the composer |
| M7 | `app.rs` `handle_talk::Partial`: also `projection.push_note(ghost)` (partials leak into the transcript) | the ghost row is CHROME, never content (F2 line stability) | KILLED — `the_ghost_row_is_chrome_never_content`: the projection entry count moved while dictating |
| M8 | `app.rs` `talk_setup_key_accepted`: reuse path returns `store = true` (re-vault the vaulted key) | the reuse path never re-stores | KILLED — `reusing_the_vaulted_key_skips_the_store`: a redundant `TranscriptionSecretStore` was issued |
| M9 | `link.rs` `CommandContext::of`: `transcription: None` unconditionally (drop the op tag) | secret RPC errors are operation-tagged, never uncorrelated | KILLED — `secret_errors_are_operation_tagged`: the error mapped to the generic `Failed { command_id: None }` |
| M10 | `haider-client/transcription.rs` `secret_from_get_response`: delete the `Error` arm (typed refusal collapses into `UnexpectedBody`) | typed daemon refusals keep their code + message | KILLED — `typed_refusals_and_skewed_bodies_map_distinctly`: the vault refusal lost its `vault_unavailable` code |

## Coverage notes

- The campaign spans every deterministic piece the brief names: wave
  ring geometry (M1), smoothing math (M2), glyph mapping (M3), the three
  talk gestures (M4, M5), generation staleness (M6), the ghost
  chrome-not-content law (M7), the setup key flow (M8), and the client/
  link RPC seam on both sides (M9, M10).
- No survivor closed a vacuous law this campaign; the T1 pattern
  (survivors exposing vacuous laws) did not recur because every T2 law
  was written against a concrete observable (a request vector, a
  composer string, a projection count, a rendered row) rather than a
  state flag alone.
- Post-campaign: `t2_wave_tests` 11/11, `t2_talk_state_tests` 23/23,
  `t2_talk_setup_tests` 15/15, `t2_talk_link_tests` 6/6,
  `transcription_tests` 4/4 — all green on the reverted tree.
