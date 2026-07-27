# W3b1 review round 3 — SHIP_WITH_FIXES

- Reviewer: gpt-5.6, 2026-07-27. Frozen SHA 15763b9 (scope 93f42a9..15763b9: W3b1.4 + W3b1.5).
- Closure: P1-1 CLOSED (registry send topology verified — the second collection is final by
  construction; the honest non-discriminating re-drain admission ACCEPTED as correct);
  P3-4 CLOSED (reviewer independently EXECUTED four mutation checks — byte-budget,
  publisher-refusal, sweep, barrier-force — all discriminated; byte-budget rewrite ruled
  "legitimate, not weaker"). P1-2 and P1-3 PARTIAL — residues are the required fixes.
- R-compliance: R1/R2/R4/R12/R16/R18 COMPLY; R3/R17/R22 PARTIAL (doc/edge precision).
- New findings: P1 none. Note: reviewer's lifecycle soak was blocked by a sandbox-wide UDS
  EPERM (confirmed environmental via an independent Ruby bind probe) — initial 34/34 passed.
- Full log: ~/haider-run/w3b1-review-r3.log

## Required fixes (completion round W3b1.6)
1. `shutdown_without_store` (runtime.rs:169/750) receives a CLONED request: a second signal
   after a Graceful clone can leave the caller Forced while daemon join reports Graceful; no
   monotonic deadline recheck. Arbitrate against the CURRENT force value + deadline.
2. barrier_step refactor regression (runtime.rs:284/547): forced-for-NoticeUndelivered is now
   raised BEFORE finalization, so an independent store flush failure gets suppressed where
   W3b1.4 reported it. Preserve store-error reporting when the notice alone forced the outcome.
3. Endpoint residual language (endpoint.rs:19/35): 0700 excludes OTHER UIDs, not the owner —
   a deliberate same-UID process CAN observe staging names via directory monitoring; strike
   "cannot even see" / "guessing is the only way" / "provably".
4. Traceability header omits connections_racing_the_shutdown_request... (33/34 mapped), and
   the xtask counter counts the test-marker TEXT inside the header comment (389 = 388 real +
   1 phantom). Add the row, make the counter syntax-aware or reword the comment, regenerate.

VERDICT: SHIP_WITH_FIXES — merge-ready once the four fixes land.
