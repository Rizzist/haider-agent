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

## Addendum (post-gate31, reviewer-authored)

Gate31 exposed two review escapes and one latent production bug, all fixed
on `w7-a` before merge:

1. **Two deleted laws restored.** The W7a compiler rewrite deleted
   `tool_result_is_presented_after_its_completed_tool_call` (caught by the
   daemond seam-sweep manifest) and
   `branch_agent_and_nonterminal_history_are_excluded_structurally` (the
   Fable D2-5 pin — vanished SILENTLY; it had no manifest entry). Both now
   live in `crates/haider-core/tests/prompt_history_tests.rs` and BOTH are
   manifest-pinned (29 entries). Doctrine: every law test earns a manifest
   coordinate, or its deletion is invisible.
2. **Gate SIGABRT root-caused: production `std::process::abort()`.**
   `bounded_response` (oauth.rs) aborted the daemon whenever a token-body
   chunk's backing buffer was still shared — hyper's connection task holds
   its read-buffer reference for a few scheduler ticks after the response
   drops, so under parallel load the "invariant" was an ordinary race.
   Diagnosis: silent exit 134 in gate31/repro; a pre-abort eprintln was
   swallowed by libtest capture and surfaced with `--nocapture`. Fix:
   bounded yield-sweep `scrub_source_chunks` — copy parse bytes, then
   scrub each chunk as it becomes exclusive; a chunk still shared at the
   bound drops unscrubbed (bounded in-process hygiene residual, journaled
   trade: the abort traded every live session for a refcount race). The
   W5b.1 source-pin law (`try_into_mut` + `drop(response)` + 3× Connection:
   close + zeroize) is preserved; a new runtime law
   (`shared_source_chunk_is_scrubbed_late_or_left_bounded_never_process_death`)
   pins no-process-death. Mutation (abort restored) EXECUTED post-commit:
   KILLED — the test binary dies SIGABRT on the held-clone segment. The
   previously-aborting binary+flags ran 6/6 clean post-fix.
3. Drain-truncation fixture raised 512→2048 (feature string growth).

Ledger 1194 → 1197.
