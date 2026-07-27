# W3c2 review round 1 — SHIP_WITH_FIXES (closing round)

Reviewer of record: Fable 5 (sole review; codex quota-dead until Aug 1). Frozen 23e3eea, scope d2daa89..23e3eea (M1 54c6e78 client+auto-spawn, M2 822ccad account machinery, M3 e1e3892 integration+carried, 23e3eea internal-verification fixes).

## Verdict summary

- **P1: none found.**
- **P2-1 (THE required fix)**: `RpcClient::request` insert-after-fail race — state read at :407 outside the pending lock; `fail()`'s one-time clear can complete between check and insert, orphaning the sender forever (no path ever completes it; every later fail is a no-op clear). One in-flight command future wedges instead of resolving `Disconnected(reason)` — on exactly the reconnect seam W3c3's LiveDriver awaits. Fix: check inside the pending lock (both directions then covered). **CLOSED by W3c2.1 (7978f56)**: check-inside-lock + executing source guard (mutation-verified) + behavioral request-after-disconnect pin.
- **P3-2** pending-command secret TTL unpinned (both enforcement sites disabled → workspace stays green; wipes lazy) → **carried to the W3c3 brief** (paused-time retryable-login test past SECRET_TTL asserting restage_required + wipe).
- **P3-3** five new stable codes not literal-pinned (constants asserted, not strings; credential_missing unasserted) → **carried to the W3c3 brief** (extend the W3c1 literal-pin precedent).
- **P3-4** Keychain/JSON vault calls run synchronously on the account actor's async task, not the blocking pool (report §1.3 directive) — degraded-not-broken (drain forces + receipts carry truth) → ledgered here; fix on next accounts touch.
- **P3-5** dead `Store::login_receipt` + adapter → **removed in W3c2.1**.
- **P3-6** unreaped race-loser child → **fixed in W3c2.1** (bounded grace poll before returning EnsuredDaemon).
- **P3-7** pre-existing environmental flake: lifecycle `reconcile_before_ready_marks_unknown_exactly_once…` StoreLocked once under full parallel load; 3/3 isolated; out of delta. Flake ledger.

## Adjudications (all ACCEPTED)

P3-2-deviation executing-guard-instead-of-schedule (wake races inside the harness actor's task; r2-precedented technique; law met). Three directed live-test changes (whitelist boots; displaced law separately pinned by production_wire_path_never_accepts_the_fake_provider). Mixed-recovery rewrite (old assertions pinned the P3-4 bug itself). 23e3eea residuals: sentinel daemon-log leg structurally uncovered (zero log/print sites on secret paths, subagent-verified); Remote-gate mutation-invisible until W3d (structural); encode_zeroizing genuinely wired on all four outbound paths; R9 1s epsilon honest.

## Mutation audit

5 executed (park→cancel revert KILLED; SecretWire Debug un-redacted KILLED TWICE — redaction pin + sentinel sweep; reconciliation vault-only arm KILLED; own pending-TTL both-sites SURVIVED → P3-2 finding; StagedSecrets claim-expiry sweep KILLED). All restored byte-identical, SHA-verified.

## Law table (all HOLD)

INV-1, INV-2, sole cursor, bounded flood, store-is-lag-buffer/fair sink admission, menu CAS, R1 seal (WorkerDependencies strictly narrower), R7 charter (zero awaits on secret routing paths), R9 deadlines (client 15s/45s + server 45s/45s on 5s ticks, paused-time exact), additive wire (fixture 28+/0−, 19/19 goldens), secret-never-persisted (sentinel sweep over all store bytes post-drain), W3b1 store-lock/endpoint-claim (client has zero unlink/kill paths).

## W3c3 readiness

RpcClient surface sufficient (request/send_frame/take_events/lost_events); login card flows expressible (stage→login, typed restage_required/busy); reconnect primitives right-shaped once P2-1 landed (it has); cursor bookkeeping stays in the driver per R11. Nothing else missing; land the code-literal pins before W3c3 clients match on them.

VERDICT: SHIP_WITH_FIXES (the enumerated fix landed as W3c2.1 — merge gate satisfied)
