# W5b review — round 4 (final close-out) — SHIP

- Frozen SHA: `645b39f` (branch `w5-b`). Method: dual review of record — gpt-5.6 (xhigh, subagents) + Fable independent confirmation + my per-crate socket gate.
- Gate (my env): per-crate serialized (bypasses a full-`--workspace` resource SIGABRT that does NOT reproduce per-binary — daemon lib 100, session_hub 33, daemond socket 86, tui 465, all crates green). fmt + whitespace clean; codex clippy `-D warnings` pass. Baseline 991.

## Verdict: SHIP

- **Round-3 P1 (worker-manager-error barrier bypass) — CLOSED.** `runtime.rs:552-650`: worker error/timeout is captured; broker + OAuth cleanup, account-actor join, hub drain, runtime drain, and finalize all execute before the sole error return. Graceful completion joins the actor task; timeout invokes `force_and_join`. `finalize` has no `?`/early return. No double join, consumed actor, or unintended deadlock. A permanently blocked vault deliberately withholds completion + lease (fail-safe).
- **Flight-lifecycle P2 — CLOSED.** `oauth.rs:2125-2187`: RAII guard armed before the first cancellable await; cancellation/rejection pointer-removes + poisons the exact flight; contenders wake failed-closed. No task/guard gap, no lock inversion.
- **Panic-ordering P2 — CLOSED.** `oauth.rs:1924-1936`: admission synchronously sealed before removal + notification; replacement work cannot enter; waiters do not hang.
- **P3 outer-admission pin — addressed** (independent pin proves rejection before any vault read).
- **Prior invariants — no regression.** Publish-before-commit (INV-1), single-flight, generation/fence, issuer/audience/resource binding, R7 (no-await routing), secret hygiene, SSRF all intact.
- `runtime.rs:479-500` is the documented process-crash seam, outside graceful/forced barrier semantics; no concrete panic bypass in the reviewed shutdown tail (ledgered boundary, not a defect).

## Round history (converging)

r1 NO_SHIP (4 P1) → W5b.1 → r2 NO_SHIP (2 P1: barrier not transitively closed; publish-before-commit; + 2 vacuous tests) → W5b.1b → r3 NO_SHIP (1 P1: worker-manager-error early-return; 2 P2) → W5b.1c → **r4 SHIP**. Barrier hole shrank each round: wide → transitive-closure → error-path-only → closed. Dual review caught 2 vacuous pins whose prior "Verified by revert" was false-confident.

## Next

Merge W5b → main. W5b.2 fills `SanctionedOAuthRegistration` (empty by default, OWNER FILL POINT) with the authorized OpenAI + Anthropic subscription constants and wires the subscription-inference adapter variants.
