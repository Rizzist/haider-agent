# TUI6 review round 6 — SHIP (wave closed)

Reviewer: gpt-5.6 (codex), frozen a5a9e0a, tight confirm of the TUI6.5 delta (4a44106..a5a9e0a).

Both r5 fix parts confirmed: (1) fresh issuance identity — counter increments only at card-open and submit; every Stage uses the submit-site remint (app.rs:2565); the r5 timeout→retype→late-old-Staged probe emits zero commands and OLD-TIMED-OUT-VAULT-REFERENCE never leaves live_pass; N/N+1/N+2 granularity storm clean, no reuse; (2) deadline-before-apply — expire_login runs before inbound apply (runtime.rs:2166) with >= LOGIN_STAGE_TIMEOUT; exact-deadline mints nothing, one-ns-inside mints legitimately (not over-closed). Three residuals ledgered with r5 reasoning. Mutation audit 2/2 killed+restored (submit-remint → r5 exploit returns; apply-before-expire → at-deadline mint). Gate: 872/872, clippy/fmt clean, ladder 16/16 (orchestrator). New findings: none at any tier. Cosmetic ledger-table nit fixed post-verdict at 3d76225.

VERDICT: SHIP

## The wave, closed

Six review rounds, every finding closed and pinned; tests 812→872; 29 executed mutation reverts. Chain: TUI6a-d (soft-wrap + band sweep + mark) → r1 NO_SHIP → TUI6.1/6.1b (resize geometry: reflow-before-input, geometry epoch, budget-across-swap) → r2 NO_SHIP → TUI6.2/2b/2c (band reserve law, surface-switch authority, TOTAL login modality) → r3 NO_SHIP → TUI6.3/6.3b (login attempt identity + paste hygiene) → r4 NO_SHIP (the 6.3b in-wave push regressed a P1 — orchestrator-owned) → TUI6.4 (identity-matched stage correlation) → r5 SHIP_WITH_FIXES (card-scoped vs stage-issuance-scoped) → TUI6.5 (stage-issuance identity + deadline ordering) → r6 SHIP.

TUI7 queue (scratchpad → to be journaled): same-key-flip flash under the card; pub screen/login type-enforcement; subagent-login+question regression pin; single-menu projection; the two P4 ledger residuals' direct pins; app.rs 3832-line split (trigger fired).
