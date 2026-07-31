# W7b-core — proactive context truth: footprint, threshold auto-compact, pre-announce

AUTHORITY: docs/research/w7-context-research.md (read WHOLE, first) — its
"W7b build order" (items 1-4, 6) binds. W7a's machinery (compiled
projection, compaction state machine, session.compact, overflow/continuation)
is SHIPPED — build ON it, never fork it.

## Scope (daemon/core/protocol/rpc only — NO haider-tui; TUI render is a
## separate Fable lane)

1. **Context-footprint accounting + honesty source.** Request-local
   accounting with a durable `ContextFootprint` snapshot (additive
   protocol event): input/output/cache token splits and a truth marker —
   EXACT when derived from provider-reported usage, ESTIMATED otherwise.
   The estimate is the compiled projection's accounting, never a guess
   labeled exact.
2. **Soft threshold before EVERY provider request.** When the resolved
   window is known: trigger auto-compaction at min(85% of window,
   window − reserved output budget) — the research's rule where a 200k
   window with a 30k reserve meets at 170k. Tools, nudges, MaxTokens
   continuations, and prior provider output all change the next request,
   so the check runs immediately before each provider round, not once
   per logical turn. Window `None` ⇒ threshold auto-compaction DISABLED
   (manual + forced overflow still work).
3. **Pre-announce.** A typed pre-announcement event (reuse/extend the
   W7a durable compaction-intent — do not invent a demo-note sibling)
   precedes `RunState::Compacting`; then the W7a compaction machinery
   runs; footprint snapshot after compaction shows the reset.
4. **Daemon authority.** No client-local threshold math in live paths;
   the daemon is the only threshold authority. Behind
   FEATURE_CONTEXT_COMPACTION_V1.
5. **Read surface for /tokens.** Expose the latest footprint (splits +
   truth marker + estimated turns-to-threshold) via the session read
   path the TUI already consumes (additive RPC payload), so the Fable
   TUI lane can render meter truth and /tokens without new wire design.

## Laws

As every lane: tests never inline (tests/ dirs or *_tests.rs siblings);
mutation docs with RUNTIME failures; CARGO_INCREMENTAL=0; fmt +
workspace clippy -D warnings clean; test haider-protocol/store/core/
daemon (sandbox socket failures expected — host gate authoritative);
ledger update via xtask test-count --update; protocol changes ADDITIVE;
regenerate goldens if manifests change; no haider-tui; no Cargo.lock;
no versions; leave changes uncommitted; no git commands.

## Tests (minimum)

- 85% threshold fires auto-compact with the pre-announce event ordered
  BEFORE `Compacting`, before the provider round (mutation: drop the
  threshold check → fails).
- Unknown window NEVER auto-compacts (mutation: substitute a default
  window → fails).
- Reserve interplay: 200k/30k meets at 170k (mutation: ignore reserve →
  fails).
- Footprint truth: EXACT only with provider usage present; ESTIMATED
  otherwise (mutation: always exact → fails).
- The check runs before every provider round — a mid-turn tool round
  that crosses the threshold compacts before the NEXT request
  (mutation: check only at turn start → fails).
- Post-compaction footprint reflects the reset (mutation: stale
  snapshot → fails).

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
