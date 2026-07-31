# W7b-core — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w7-b`, reviewed at 3b29e16 (frozen ref).
Implementer: codex lane (gpt-5.6 xhigh) per docs/briefs/W7b-context-ux-brief.md.

## What shipped

Durable `ContextFootprint` snapshots (input/output/cache splits) with a
truth marker that is EXACT only for provider-reported usage (cache-subset
subtraction guarded; anything else degrades to ESTIMATED from the compiled
projection's accounting). Soft threshold `min(85% of window, window −
reserved output)` enforced inside the `'requests` loop — before EVERY
provider round (tool rounds, nudges, MaxTokens continuations all re-check).
Unknown window ⇒ threshold disabled, estimates still published. Pre-announce
= the W7a typed compaction-intent marker committed BEFORE
`RunState::Compacting` (no demo-note sibling invented). Post-compaction
reset snapshot. `session.read` exposes the head-fenced latest footprint.
Feature-gated under FEATURE_CONTEXT_COMPACTION_V1. Ledger 1197 → 1207.

## Mutations (reviewer-chosen, EXECUTED post-commit at 3b29e16)

| # | Mutation | Result |
|---|---|---|
| M1 | threshold drops `.min(hard_fit)` | KILLED (`soft_threshold_honors_eighty_five_percent_and_output_reserve` — the 40k-reserve case is non-degenerate, so the pin is honest; 200k/30k alone would have been blind) |
| M2 | policy check skipped when `provider_attempt > 0` | SURVIVED — ISOLATED: invalid mutation, `provider_attempt` resets to 0 on tool rounds/continuations, so the guard never skipped a real round. Re-targeted as a once-per-turn latch (M2b): KILLED by three independent pins (tool-round crossing, continuation growth, hard-fit recheck) |
| M3 | unreported usage claims EXACT | KILLED (`footprint_is_exact_only_for_request_local_provider_usage`) |
| M4 | pre-announce loses its typed kind | KILLED (ordering pin + overflow path both fail) |
| M5 | unknown window defaults to 200k | KILLED (2 pins) |

## Honest residuals (non-blocking)

- `latest_context_footprint` (session_hub/rpc.rs) scans the journal from
  seq 0 on every `session.read` — O(session length). Fine at current
  session sizes; an indexed latest-footprint lookup is a later perf pass.
- No live probe yet exercises threshold auto-compaction (needs a real
  near-window session); W7b-tui adds the live compact probe via /compact.

## Gate

gate33: full per-crate gate GREEN (fail=0) — protocol 25, core 60, daemon 199, daemond 86, rpc 55, tui 537, all others clean. Workspace clippy -D warnings clean. Verdict: SHIP (merges with the W7b-tui lane as v0.0.35).
