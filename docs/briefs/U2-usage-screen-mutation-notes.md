# U2 — `/usage` screen — executed mutation ledger

Protocol per kill: apply the mutation, run the named law ("running 1
test" observed), record the runtime failure verbatim, revert, re-run
green. Full `u2_usage_screen_tests.rs` (18 laws) green after the final
revert; full workspace green before the campaign.

## 8 executions — 7 kills + 1 survivor closed (then killed)

1. **Mutation:** `usage_bar` fill switched floor → `.round()` —
   `haider-tui/src/format.rs`. KILLED by
   `usage_bar_math_clamps_and_floors`:
   `assertion 'left == right' failed: FLOOR, never round: 0.87 stays at
   8 cells (rounding would claim 9) — left: "▰▰▰▰▰▰▰▰▰▱", right:
   "▰▰▰▰▰▰▰▰▱▱"`. (The 0.96 case alone would have been RESCUED by the
   never-reads-full honesty clamp — 0.87 is the case the clamp cannot
   save, added before execution.) Reverted, green.
2. **Mutation:** `fmt_reset` day tier dropped (`let days = 0;`) —
   `haider-tui/src/format.rs`. KILLED by `reset_times_format_by_tier`:
   `assertion 'left == right' failed: day tier drops minutes —
   left: "resets in 3h 0m", right: "resets in 5d 3h"` (the 5d3h delta
   collapsed onto the modulo hours). Reverted, green.
3. **Mutation (owner-mandated):** `mask_identity` returns its input
   verbatim — `haider-tui/src/format.rs`. KILLED TWICE: by
   `identity_masking_keeps_first_chars_and_tld_only`
   (`left: "support@diffforge.ai", right: "s******@d********.ai"`) AND
   by the render-level
   `identities_render_masked_by_default_and_reveal_is_per_visit`
   (`panicked: the raw email never renders on open`) — the leak is
   caught at the helper and at the screen. Reverted, green.
4. **Mutation:** the renderer's `Unavailable` arm replaced with a zeroed
   fabricated bar — `haider-tui/src/render.rs` `render_usage`. KILLED by
   `unavailable_meters_render_the_typed_reason_never_a_bar`:
   `panicked: the typed reason renders honestly` (and the second assert
   — no `▰`/`▱` in the body — would fire next). Reverted, green.
5. **Mutation:** the mask reset dropped from `enter_usage` —
   `haider-tui/src/app.rs`. **SURVIVED the original law** — the esc exit
   (`exit_usage`) carries its own reset, and the law only ever left via
   esc. Honest gap: the enter-door reset is what covers exits that
   BYPASS `exit_usage` (⌃C `back_to_launcher`, surface switches). Law
   strengthened with a Sub-Escape-Lane (reveal → ⌃C out → reopen) and
   the mutation re-executed: KILLED by
   `identities_render_masked_by_default_and_reveal_is_per_visit`:
   `panicked: the visit after a ⌃C exit STILL opens masked`. Reverted,
   green.
6. **Mutation:** the provider filter's `starts_with` → `contains` —
   `haider-tui/src/app.rs` `UsageState::groups`. KILLED by
   `usage_filter_shows_only_the_named_provider`:
   `panicked: the filter is a PREFIX, never a substring` (`/usage oauth`
   must match nothing — `oauth` is a mid-string fragment of
   `anthropic-oauth`, not a provider). Reverted, green.
7. **Mutation:** `UsageState::apply_report` keeps `fetching = true`
   (the clear removed) — `haider-tui/src/app.rs`. KILLED by
   `live_replies_install_the_report_and_failures_land_typed`:
   `panicked: …and the in-flight mark clears`. Reverted, green.
8. **Mutation:** the render's scroll application pinned to zero
   (`Paragraph::scroll((0, 0))`) — `haider-tui/src/render.rs`
   `render_usage`. KILLED by `long_reports_scroll_to_reach_every_line`:
   `panicked: End reaches the last provider block` (the frame writes
   `scroll_max` but the viewport never moves — the F2b reachability law
   is what notices). Reverted, green.

## Verdicts

| # | Seam | Verdict |
|---|---|---|
| 1 | bar fill math | KILLED (law pre-strengthened past the honesty-clamp rescue) |
| 2 | reset-time day tier | KILLED |
| 3 | identity mask (owner-mandated) | KILLED at helper AND screen |
| 4 | unavailable → fabricated bar | KILLED |
| 5 | mask reset on open | SURVIVED → law strengthened (⌃C lane) → KILLED |
| 6 | filter prefix law | KILLED |
| 7 | install clears in-flight | KILLED |
| 8 | F2b scroll application | KILLED |
