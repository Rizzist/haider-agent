# S4 — subagent rows: right-aligned `elapsed · ↓ tokens` — notes

Implementer: Fable 5. Branch `s4-subagent-rows` from main @ `3cd8e99`.

Owner directive (Claude Code screenshot as the model): subtree chip rows
get a RIGHT-ALIGNED `elapsed · ↓ tokens` meta — e.g. `25m 18s · ↓ 266k
tokens` — live children ticking on the EXISTING anim clock, terminal
children frozen at their final duration, tokens = the child's total.

## Surface

One implementation: `render_subtree` (`render.rs`) — the panel shared by
the session and subagent screens, so every "row/chip" wears the meta from
one seam. The meta rides the DIM theme slot (`theme.dim_style()`), padded
to the row's right edge with a ≥2-cell gap before it; the hover band and
hit rects are untouched (the pad is inside the same Line).

## Elapsed — journal timestamps, one stopping law

- `ChipModel` gains `spawned_at_ms` / `last_event_at_ms` (`app.rs`).
  Spawn instant = the `AgentSpawned` envelope's `committed_at_ms`.
  `note_event_at` advances the event clock (monotone max) from EVERY
  child-attributed envelope — and REFUSES once the chip is terminal
  (`elapsed_frozen` = `!is_live()`, deliberately the same law the tree
  counts liveness by). `set_state_at` notes-then-flips, so a terminal
  `AgentChipState` freezes the clock at its own timestamp: `last −
  spawned` IS the frozen final, with no third field to drift. Replay
  re-derives identical figures from the same journal timestamps.
- Threading: `committed_at_ms` now rides the routing seams —
  `AppModel::route_admitted/absorb_scoped` (attached),
  `SessionState::route_admitted/absorb_scoped` (background),
  `branch::absorb_into_view` (parked) — into `apply_agent_payload` /
  `chip_apply`, so all four routes speak one clock. The demo's two chip
  arms (active `consume` + `absorb_demo_event`) stamp `now_epoch_ms()`:
  the demo fabricates locally, so its journal time IS the wall clock.
  Demo-store `ChipDto` persists both instants additively (pre-S4 files
  load with no time base → no segment).
- Render clock: `AppModel::clock_ms`, advanced by the EXISTING anim tick
  (both run loops — no new timer) and by every applied envelope's
  `committed_at_ms` (`route_raw`), so the first paint after a spawn is
  already inside the journal's time base. `animated()` gains a
  `tree_live_count > 0` arm on the session/subagent screens: a live
  idle/waiting child must tick even though it is outside the pulse set
  (the derived WAITING badge overlays IDLE only — a STREAMING parent
  with an idle child had no wakeup). Terminal chips read frozen journal
  time and keep the gate closed.
- Format law (`format.rs::fmt_elapsed`): h/m/s tiers — `42s`, `25m 18s`,
  `1h 4m 9s` — units descend, no zero-padding, lower units always present
  in-tier, seconds truncate (live tick can never overshoot the frozen
  final).

## Tokens — the honest source (investigated)

The directive named two candidate sources and asked which is real:

1. **Parent-view child-attributed `Usage` fold — NOT real on live
   streams.** Verified against the daemon: the delegation mirror
   (`haider-daemon/src/delegation.rs::mirror_child_chip_states`) projects
   ONLY child `UserMessage` (prompts/steers) and `RunState`→
   `AgentChipState` into the parent journal; `derive_terminal_report`
   adds `AgentReport`. No child `Usage`, no footprint extension items
   ever ride the parent stream — folding it would render a fabricated
   `0` forever. (The demo is the exception: `ChipTokens` feeds
   `chip.tokens` directly, which is why that counter stays in the chain.)
2. **Roster join via manifest `child_session_id` — real.** Children are
   full sessions; `delegation.rs::establish` writes `child_session_id`
   into the manifest's reserved `coordinates`, the store's session list
   includes child sessions, and `SessionSummary::footprint_tokens` is
   computed from the child's own sealed journal (the same roster truth
   the launcher shows). Limit, documented honestly: `session.list` runs
   at boot/resume, so a child spawned mid-session joins nothing until a
   reconnect — its row shows elapsed only, which is honest, not wrong.

Chosen chain (`app.rs::chip_row_tokens`), truth-ordered:
chip transcript's own durable footprint (shared FIRST source with the
`/tokens` panel, so the two surfaces cannot disagree) → `chip.tokens`
when it has accrued (demo feed) → the roster join
(`SessionState::known_tokens`: fresher-summary → applied projection
usage → stale summary → `None`). `None` at the end of the chain DROPS
the segment — unknown is never rendered as zero.

Join-correctness law: exact-match on the chip's OWN recorded
`child_session` — never positional, never by callsign.

Token format reuses the ONE shared `fmt_tok` (sim `fmtTok` parity):
`265_900 → 266k`, not the screenshot's `265.9k` — a second token dialect
an inch above the status bar's `266k` would make the same number render
two ways. The k/M unit law is what is pinned; the formatter stays single.

## Width degradation (F2c pattern, law-pinned)

`render.rs::chip_row_meta`: candidates are WHOLE-segment strings —
`elapsed · ↓ N tokens` → `elapsed` → nothing; tokens drop FIRST. Segment
PRESENCE is data truth (missing source = missing segment, no degradation
involved); never a mid-segment truncation.

## Tests + rituals

`tests/s4_subagent_rows_tests.rs` (12): format tiers, live tick +
right-alignment + pad gap, clock-from-envelopes, anim-gate law, frozen
final at the terminal envelope, post-Done report never moves the final,
chip-clock unit laws, join correctness (two chips × two summaries),
unknown-renders-nothing, truth order, whole-segment degradation at
118/76/62 cols, dim-slot ink. Full `cargo test -p haider-tui` green;
ladder 16/16; mutation notes: `S4-subagent-rows-mutation-notes.md`
(6 EXECUTED kills).
