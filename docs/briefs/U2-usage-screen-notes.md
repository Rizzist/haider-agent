# U2 — `/usage` cross-provider usage screen — notes

Lane U2, branch `u2-usage-screen` from main @ 53b51ee. Scope: the
full-screen `/usage` TUI consuming U1's `usage.report` RPC (feature
`usage_report_v1`) — every wire shape CONSUMED from `haider-protocol` /
`haider-rpc`, never redefined. Owner contract: Claude Code's `/usage`,
cross-provider; plus the owner addendum (identity masking, below).

## Shipped

- **Screen** — `Screen::Usage` + `UsageState` (`app.rs`): the
  `usage.report` snapshot, provider-prefix filter, provider-group cursor,
  per-provider account tabs, F2b scroll cells. One door in
  (`enter_usage`) — feature-gated BEFORE anything opens (the B2b lesson;
  an ungated daemon gets the honest stale-daemon note), demo opens an
  honest EMPTY state (usage is daemon truth — the demo fabricates no
  meter, the hooks precedent). Esc routes back session-else-launcher (the
  `/accounts` law).
- **Per-state rendering** (`render_usage`, `render.rs`) — the wire's tag
  decides, never a re-judgement:
  - `metered`: one bar per window — `usage_bar` (floor-fill + honesty
    clamps) on theme slots (`ok`/`warn`/`err` by the pinned 0.70/0.90
    thresholds), `fmt_pct`, and `fmt_reset` against the report's OWN
    `generated_at_ms` (both instants ride the daemon clock — the line is
    a pure function of the snapshot; no wall-clock in render). Named
    extra limits use their `label` over the window key.
  - `unavailable`: the typed reason in warn ink — NEVER a fabricated bar
    (law-pinned: no meter glyph in the body when every shown meter is
    unavailable/local-only).
  - `local_only` (API key / custom): an explicit "api key — no provider
    meter" note, tokens + est cost only — no bar, no percent.
  - Every account: (masked) identity · plan · auth flavor; local journal
    stats (sessions · duration · est cost · ±LOC, then the four token
    splits); `est —` when no priced model matched (never $0.00).
  - `THIS DEVICE` totals over the SHOWN accounts — safe to sum because
    U1 attributes sessions/duration/LOC to exactly one dominant account.
- **Identity masking (owner addendum)** — `mask_identity` (`format.rs`):
  first char of local part + first char of domain survive, `*` for the
  rest, capped at eight stars per run so long identities do not reveal
  their exact length; final `.tld` readable
  (`support@diffforge.ai` → `s******@d********.ai`); non-emails mask as
  one part. MASKED BY DEFAULT on every open (`enter_usage` resets the
  flag — a reveal can never survive into a later visit whichever way the
  last one ended); `r` toggles the reveal for the CURRENT visit; esc
  resets again on the way out.
- **Navigation** — ↑/↓ provider-group cursor (F2b follow-latch), ←/→
  (and ⇥/⇧⇥) cycle the cursor group's accounts WRAPPING (same provider
  only), account tab chips are value-carrying `Hit::UsageAccountTab`
  click targets, PageUp/PageDown ±8, Home/End, wheel ±3 — RENDER is the
  single scroll authority (frame-written `scroll_max`, `⋮` edge marks,
  pinned footer hint ≥ 12 rows, flowed under).
- **Filter** — `/usage <provider>`: case-insensitive PREFIX match
  (`anthropic` catches `anthropic-oauth`); unknown filters render the
  honest "no accounts match" note; bare `/usage` clears. Re-running
  `/usage` while open re-filters (and live re-reads).
- **Registry** (`commands.rs`) — `/usage [provider]` registered; the F2a
  no-hijack law: ABSENT from `has_arg_slots` (⏎ on the palette row RUNS
  it), present in `offers_arg_completions`; the filter slot completes
  from the discovered provider roster (`DynamicSlots::providers`).
  `/help` line added.
- **Wire plumbing** — `AppRequest::UsageRefresh` →
  `LiveCommand::UsageReport` (a READ: no durable id, never outboxed —
  the `hooks.list` discipline) → `RequestBody::UsageReport` →
  `ResponseBody::UsageReport { report }` → `LiveReply::UsageReport`
  (Boxed) installed whole by the ONE writer (`UsageState::apply_report`).
  Errors: the read has no command id, so the link's `CommandContext`
  carries a `usage_report` tag and the no-id error decodes to
  `LiveReply::UsageReportFailed` — the typed message lands ON the usage
  screen (`ⅹ usage read failed — …`) and never erases held truth. `f`
  re-reads; demo driver lists `UsageRefresh` under live-only vocabulary.

## Laws (ledger 1883 → 1901, +18; all in `u2_usage_screen_tests.rs`)

1. `usage_bar_math_clamps_and_floors` — 0–1 → cells: clamp both ends,
   floor fill, nonzero shows ≥ 1 cell, sub-1.0 never reads full; bar
   width + percent rounding/clamping pinned.
2. `reset_times_format_by_tier` — soon (elapsed/past/sub-minute) · `{m}m`
   · `{h}h {m}m` · `{d}d {h}h`; minutes floor.
3. `usage_tone_thresholds_are_pinned` — ok < 0.70 ≤ warn < 0.90 ≤ err,
   clamped input.
4. `identity_masking_keeps_first_chars_and_tld_only` — the owner
   addendum's exact example + never-leaks-the-local-part.
5. `metered_accounts_render_bars_percent_and_resets` — bars, %, reset
   times, plan, auth flavor, local stats, token splits, LOC.
6. `unavailable_meters_render_the_typed_reason_never_a_bar` — reason
   renders; NO meter glyph in the body.
7. `api_key_accounts_render_tokens_and_cost_never_a_meter` — the
   no-server-meter note, est cost, tokens; no bar/percent.
8. `missing_cost_estimates_render_a_dash_never_zero`.
9. `identities_render_masked_by_default_and_reveal_is_per_visit` —
   masked on open, `r` reveals, close+reopen masks again.
10. `usage_filter_shows_only_the_named_provider` — prefix filter,
    honest unknown-filter note, bare clears.
11. `left_right_tabs_cycle_the_cursor_groups_accounts_and_wrap` — wraps
    within the SAME provider; one-account groups never cycle; tab-chip
    click law.
12. `usage_owns_its_keys_esc_closes_and_enter_never_hijacks` — F2a key
    ownership: ⏎ inert, strays swallowed (no composer echo), esc closes.
13. `long_reports_scroll_to_reach_every_line` — F2b: End reaches the
    tail, Home restores, PageDown 8, wheel 3, clamp at frame-written
    max, follow-latch consumed by the frame.
14. `usage_is_registered_and_enter_runs_it_without_arg_hijack` —
    registry presence, `!has_arg_slots`, completions, arg rows.
15. `demo_usage_opens_an_honest_empty_state` — no fabricated report, no
    read pushed, the note names live mode.
16. `live_usage_entry_is_feature_gated_then_fetches` — ungated flashes
    and does NOT open; gated opens fetching + pushes; `f` fetches, `r`
    never does.
17. `live_replies_install_the_report_and_failures_land_typed` — driver
    mapping, read-has-no-id, install clears fetching, failure lands
    typed and keeps held truth.
18. `usage_wire_bodies_and_replies_map_onto_u1s_shapes` — exact
    `usage.report` request JSON; snapshot + identity-tagged no-id error
    decode.

## Gate

`cargo fmt --all` clean; `cargo clippy -p haider-tui --all-targets`
clean; full workspace test run green (`CARGO_INCREMENTAL=0`,
`ulimit -n 8192`); ladder 16/16 (14 demo + 2 live); ledger
`cargo run -p xtask -- test-count --update` → **1901**. No version
bumps, no tags, Cargo.lock untouched. Executed mutation campaign in
`U2-usage-screen-mutation-notes.md`: 8 executions — 7 kills + 1 survivor
(the enter-door mask reset, rescued by the esc path's own reset) closed
by strengthening the law with a ⌃C exit lane, then killed.

## Not in this wave / honest limits

- No auto-refresh/polling: the read rides screen entry + `f` (U1's
  daemon-side poll floors make a client poll pointless inside them).
- A socket loss mid-read leaves the honest "fetching usage…" note until
  `f` (no reconnect re-issue seam for screen-scoped reads; same posture
  as `/hooks`).
- Reset instants render relative to the report's `generated_at_ms`, so a
  long-held snapshot ages honestly ("resets in…" as of the snapshot);
  `f` is the remedy, and determinism in render was worth more than a
  wall-clock delta.
- The `THIS DEVICE` totals sum only the SHOWN (filtered) accounts, and
  say so in the header.
- Window names render the provider's own vocabulary (`five_hour`,
  `seven_day`, wham's `primary`/`secondary`, kimi's rolling windows) —
  no client-side renaming table; the label field wins when U1 supplies
  one.
