# W7a — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w7-a`, reviewed at a8418a0 (frozen ref).
Implementer: codex lane (gpt-5.6 xhigh) per
docs/briefs/W7a-context-core-brief.md, mapped by
docs/research/w7-context-research.md.

## What shipped — the compiled-projection law goes live

The provider prompt is now the COMPILED PROJECTION of the durable tree,
proven byte-identical to the old journal rendering (text, tool calls,
tool results, provider-opaque fragments). Compaction is an immutable
ancestry substitution behind a durable INTENT (crash mid-compaction
abandons or completes; a missing summary artifact is typed store
corruption; the commit is CAS-aware against concurrent turns).
`session.compact` joins the RPC surface, receipt-backed and
replay-idempotent. Provider context-exceeded is a DISTINCT
classification with fixture shapes for both vendors (Anthropic's
max_tokens conflation disambiguated); overflow forces one compaction
and one retry inside the same logical turn. MaxTokens no longer ends a
run: bounded continuation (default 8/turn), including the hard-fit
recheck that compacts BEFORE continuing when the input budget is blown.
The resolved turn provider carries the catalog window + the
daemon-owned reserved output budget.

## Mutations (reviewer-chosen, EXECUTED post-commit at a8418a0)

| # | Mutation | Result |
|---|---|---|
| M1 | compiler drops provider-opaque fragments | SURVIVED the byte-equivalence pin (BOTH compile paths share the hydration — A-vs-B comparisons are blind to shared-path mutations), KILLED by the content-presence pin. Both pin layers are necessary; journaled as review doctrine |
| M2 | substitution keeps the covered prefix | KILLED (suffix-only pin) |
| M3 | overflow classified generic | KILLED (vendor fixture pin) |
| M4 | continuation cap enforcement removed | const-mutation was invisible (the pin configures its own cap — correctly testing the MECHANISM); re-targeted at the enforcement branch and KILLED (`repeated_max_tokens_is_bounded_independently`) |

## Gate

Workspace clippy `-D warnings` clean; six host suites green (415
passed, sockets included); full per-crate gate `gate30.out`; ledger
1179 → 1194.

## Honest residuals (non-blocking → W7b)

- No threshold auto-compact yet — compaction fires on overflow, hard-fit
  recheck, or the manual RPC; the 85% pre-announce trigger is W7b.
- The TUI's /compact still routes to the demo beats in live mode until
  W7b wires the RPC; meter exact-vs-estimated truth also W7b.
- No live probe yet exercises a REAL overflow (272k of input is an
  expensive fixture); the vendor fixtures stand in until W7b's live
  compact probe.

## Verdict

**SHIP** (merge to main, ships as v0.0.34).
