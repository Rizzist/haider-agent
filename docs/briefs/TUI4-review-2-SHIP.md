# TUI4 arc delta review round 2 — SHIP

- Reviewer of record: Fable 5 (two gpt attempts died on platform errors — content-filter false positive on demo copy, then a rollout-thread infra fault; neither produced a verdict).
- Scope 906636f..ac5e908 (the TUI4.1 fix commit), frozen at ac5e908, worktree restored byte-identical.
- **All 11 round-1 findings CLOSED** (5 gpt + 5 Fable D2 + D3-3), each with both-sides file:line evidence and re-executed PTY reproductions:
  - P1-1 strict hydration: version-first guard + deny_unknown_fields on all 9 DTOs + strict SessionDto core (`head`'s lone default verified as genuine sim law, tui.js:725); version-999 and partial-sessions repros fall back to seeds; the interior-defaults tier survived construction attacks (malformed interiors reject whole-file; schema-valid degenerate interiors render safely — option_count>0 guards + .get()).
  - P1-2 monotonic identity: single mint site, /reset never rewinds, hydrate maxes upward; session_epoch deleted for derived session_identity(); class hunt found AutoTitle is the only surviving id-keyed control callback and the seed-remint recurrence is double-gated (seeds mint titled + title.is_none() landing gates).
  - P2-3 probes: 12/12 ladder PASS under hostile NO_COLOR=1 CLICOLOR=0; enforceability PROVEN (three deliberate breaks all exit nonzero, incl. the never-alt-screen dead-process hole).
  - P2-4/D2-5 W3c seam ledgered (8-point row verified undiluted); P3-5 clipboard bounded-exit truth; D2-1/2/3/4 all closed; D3-3 id-0 rejected.
- Mutation audit: 5 executed (version guard, serde-default restore, id-0 clause, next_session_id rewind → both law tests, auto-title arm ownership) — all failed as required, restored byte-identical each time.
- Test integrity: pure move TRUE, zero deletions, +5 law tests, 523→528 independently recomputed; three additional Hasan→Husayn retargets are strictness-preserving consequences of the D2-2 rewiring (commit undercounts; nothing weakened).
- Full gate: 527 passed/0 failed, clippy -D warnings, fmt, xtask 528/528.
- Merge readiness: merge-tree vs main (12bb3a6) → ONLY test-baseline.txt conflicts (530 vs 528); regenerate post-merge (expected ~581, command authoritative).

## Carried P3s (next TUI round's ledger — hand-edit/harness tier, none user-reachable)

1. **P3-1** hydrate guard-2 upper bound: persisted id u64::MAX → debug overflow panic at app.rs:2264 / release wrap back onto the id-0 sentinel (one-line fix: reject id == u64::MAX beside the == 0 clause, or saturating increments; matching bound on card_seq).
2. **P3-2** scratch identity 0 exempt from monotonicity: stale origin-0 AutoTitle can title the NEXT scratch across fresh_session (harness/plain-oracle lineage only; document the scratch exemption on session_identity()).
3. **P3-3** hydrate accepts duplicate session ids (cosmetic mirror-corruption on save; fold into the P3-1 load clause).
4. **P3-4** anim probe SKIPs ink/liveness on row_visible even at the declared 118×36 gate size (consider fail-not-skip at gate size).
5. **P3-5** copy_local's 300ms bounded poll runs on the event loop (documented; revisit W3c era).

VERDICT: SHIP
