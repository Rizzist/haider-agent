# W7b-tui — review of record #1 — SHIP

Reviewer + implementer: Fable 5 (UI lane never goes to codex). Branch
`w7-b`, reviewed at 880b57a.

## What shipped

- **Live `/compact`** routes to the receipt-backed idle-only
  `session.compact` (LiveCommand::Compact → RequestBody::SessionCompact →
  LiveReply::Compacted retires the outbox entry). The P1-A anti-wedge law
  EVOLVED, not retired: nothing local is fabricated — the daemon's
  committed events own every visible state change. `/compact` leaves the
  demo-only vocabulary (test renamed
  `live_compact_routes_to_the_daemon_and_fabricates_nothing`).
- **Footprint consumption**: `context_footprint_v1` extension items are
  CONSUMED by the projection (never `⋯` transcript rows — one arrives per
  provider round); the `context_compaction_intent_v1` marker becomes the
  pre-announce note with the live percent (sim §Q2 vocabulary).
- **Meter truth**: the status-bar meter prefers the durable occupancy
  snapshot over the cumulative usage sum; ESTIMATED wears `~`, EXACT is
  plain; the snapshot's own window wins over the identity fallback.
- **⌃G / `/tokens`**: the real panel (sim tui.js:2946-2977) — main row
  from the footprint (splits + ≈turns-to-auto-compaction), chip rows,
  esc closes; demo mode keeps the sim's fabricated 62/28/10 split.
- **`/tree`**: main-line view (sim tui.js:3366-3430 vocabulary): branch
  header, `├─ ❯` user turns, `⊟ compacted N → M` nodes, windowed
  selection; forks/jump stay flagged for the branch wave.

## Fixture evolutions (stubs became real)

palette-enter, palette-click, stale-hit, ⌃G-stub, demo-refusal — each
updated to assert the STRONGER real effect (e.g. Screen::Tree instead of
a stub flash). `/fork` keeps carrying the stub-honesty law.

## Mutations (EXECUTED post-commit at 880b57a) — 5/5 KILLED

| # | Mutation | Killed by |
|---|---|---|
| T1 | consume_context_extension guard dropped | 4 tests (⋯ row leaks + meter dead) |
| T2 | meter ignores truth (always ~) | the tilde law |
| T3 | panel ignores truth (always ~) | the panel truth law |
| T4 | tree drops compaction nodes | the tree law |
| T5 | live routing arm dropped | the P1-A routing law |

## Gate

gate34: full per-crate gate GREEN (fail=0) — tui 542, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (v0.0.35). · ledger 1207 → 1212.
