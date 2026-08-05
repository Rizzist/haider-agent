# F2 — TUI model picker wave — mutation notes

Implementer: Fable 5. Branch `f2-tui-model-picker`. Every mutation below
was EXECUTED against the completed tree: apply the mutation, run the
named suite (`running 1 test` evidence captured), record the observed
runtime failure, revert with `git checkout`, re-run green. One mutation
SURVIVED and forced a new law — recorded honestly below.

## Executed runtime kills

| # | Mutation (seam) | Class | Expected law | Observed kill |
|---|---|---|---|---|
| M1 | Fence delimiter lines CONSUMED: `render_markdown` toggles fence state but skips the push (`md.rs`) | line-geometry | `line_stability_one_rendered_line_per_source_line` | KILLED — `left: 3, right: 5` naming the fenced corpus sample: the ```` ``` ```` lines vanished and every anchor below them would shift |
| M2 | Bold arm eats content: `chars[i + 3..close]` instead of `i + 2` (`md.rs`) | byte preservation | `byte_content_preservation_only_matched_markers_are_consumed` | KILLED — `non-marker '9' went missing from "the answer is **97** exactly"` — the subsequence walk names the exact eaten character |
| M3 | Styled walker budget shrunk by one: `budget.max(2) - 1` (`md.rs` `wrap_spans`) | vacuous-pin hunt | `styled_wrap_never_moves_a_break` | **SURVIVED** (16/16 green) — the parity law compares the styled walk against the SAME mutated walker on plain spans, so a global budget shift is invisible to it. Gap closed: `styled_wrap_matches_the_hand_computed_oracle` (independent literal rows, e.g. `["the answer is 97 ", "exactly"]` at width 17). Re-applied mutant: KILLED — `left: ["the answer is 97", " exactly"]` breaking one cell early. Reverted, 17/17 green |
| M4 | `/model` restored to `has_arg_slots` (`commands.rs`) | heeded-history regression | `exact_model_enter_opens_the_picker_not_an_arg_slot` | KILLED — `left: ["claude-opus-5", "claude-sonnet-5"], right: ["/model"]` — the EXACT historic hijack reproduced: arg rows under ⏎, the picker never opens |
| M5 | Resolved pair replaced by request echo: the `ModelSelected` arm applies the picker's `pending` pair instead of the reply's (`live.rs`) | no-echo / R2 | `live_selection_is_receipted_and_renders_the_resolved_pair` | KILLED — `RESOLVED provider: left: "openai", right: "openai-oauth"` — the test's deliberately-different resolved provider catches any echo |
| M6 | `record_session_error` dropped from the rejected-submit arm; flash-only again (`live.rs`) | silent-IDLE class | `a_rejected_submit_lands_in_the_session_view` | KILLED — `the public reason reaches the session view: []` — the transcript is empty exactly as the pre-sweep UI was |
| M7 | Unpaired-Errored synthesizer deleted from the RunState arm (`projection.rs`) | silent-IDLE class | `errored_without_a_paired_reason_synthesizes_a_line` | KILLED — `exactly one synthesized line: left: 0, right: 1` — badge-only ✗ ERRORED with an empty transcript (the pre-W5g-6 owner bug resurrected) |
| M8 | Roster paint ignores the offset: `Paragraph::new(lines)` without `.scroll` (`render.rs`) | degenerate-fixture hunt | `long_rosters_scroll_to_reach_every_row` | KILLED — `End reaches the roster's last provider` fails: `synth-11` never enters the viewport even at max scroll (companion `cursor_walk_keeps_the_selected_provider_visible` dies with it) |
| M9 | Width degradation replaced by mid-word truncation to budget (`app.rs` `composer_identity`) | degradation law | `width_degradation_drops_whole_segments_in_order` | KILLED — `left: Some("fable-5 · oauth · high · fas"), right: Some("fable-5 · oauth")` — the literal mid-word garbage the owner contract forbids, on screen in the assertion |
| M10 | Footer never pins: `let pinned = false` (`render.rs`) | owner-contract | `add_login_buttons_pin_at_the_bottom` | KILLED — `"+ OpenAI (OAuth)" must be visible without scrolling` — the buttons sank below the fold of the overflowing roster |

## Verdicts

- 9 of 10 mutants killed at first contact; M3 survived and was the
  campaign's payoff: the wrap-parity law was self-referential (both
  sides of the equality flow through the mutated walker). The
  hand-computed literal oracle is now the anti-vacuity anchor for the
  whole wrap seam; ledger 1703 → 1704 with it.
- The M2/M9 kills print the exact corrupted bytes ("'9' went missing",
  `"… · fas"`) — the failure messages themselves prove the laws observe
  real renderer output, not fixture echoes.
- After the final revert: full `cargo test -p haider-tui` green (74
  suites), `cargo clippy --all-targets` clean, tree clean.

## Review of record (coordinator, executed post-lane)

| # | Mutation (seam) | Law | Observed kill |
|---|---|---|---|
| RV1 | Search filter matches everything: token walk replaced by `true` (`app.rs` `model_picker_filtered`) | `search_matches_model_and_provider_case_insensitively` | KILLED — running 1 test → FAILED at the exclusion assert: the law pins that non-matching rows are ABSENT, so a match-all filter cannot pass (vacuity check on the owner's "even search" contract) |

Reverted; suite green. Lane's 10-mutation campaign reviewed: M5's
deliberately-different resolved provider and M3's hand-computed oracle close
the echo and self-referential classes; no further gaps found.
