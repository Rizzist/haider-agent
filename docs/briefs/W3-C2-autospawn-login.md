# W3c2 — reusable client, auto-spawn, and `/login` (the keystone's front door)

Lane branch: `w3-c2` (worktree `~/haider-run/haider-w3c`, base 4dfcfc3 = merged W3c1 turn engine).
Goal: `haider` connects to (or spawns) the real daemon, and `/login anthropic api` puts a working credential where the next real turn picks it up. After this chunk, W3c3 swaps the TUI onto the live wire and v0.0.12 ships the keystone.

## Binding law (precedence order)

1. `docs/research/w3c-research-report.md` **§6.2 (this chunk's scope + tests, verbatim)** + §3 R7/R8/R9/R10 (all DECIDED — implement, don't re-litigate). Report wins.
2. `docs/briefs/W3c1-review-2-SHIP_WITH_FIXES.md` — the W3c2 seam report (what auto-spawn/login touch; the receipt/provider seams confirmed READY) and the carried notes this chunk owns.
3. d1 report + the six invariants + the W3b1 endpoint-claim/store-lock laws (auto-spawn arbitration builds on them).
4. `docs/OPTIMIZATIONS.md` — the fence-vs-replay row's trigger FIRES this chunk (first non-wire receipt caller): resolve it as the row directs.

## Scope (report §6.2 verbatim, plus the carried items this chunk owns)

- **Client**: shared `ResolvedProfile`; reusable UDS `RpcClient` (in haider-rpc or a thin client module — NOT in the TUI); pending-request correlation, bounded writer/reader, ping, reconnect primitives.
- **Auto-spawn**: bare `haider` connects first, spawns detached `haiderd` only on missing/refused endpoint, handshake-polls; store-lock elects concurrent-launch winners (loser child exits 75, both parents reach Ready); stale-owner-socket recovery by the winner only; live-but-old daemon is NEVER killed or replaced (explicit feature/version-skew diagnostics); parent exit leaves the daemon running; owner-only daemon log.
- **Login**: daemon account actor (connection routing hands login off, never awaits inline — R7); sensitive same-UID UDS stage codec with bounded command-owned secret lifetime; `vault.stage`/`account.login_api`/`account.list` frames + goldens + feature strings (pure additive under the Unknown-tolerant rules); fake-injectable `CredentialValidator`; parent-directory-fsynced profile-namespaced account persistence (Keychain path is macOS — the report's R10 gate); **pending/committed-login receipt reconciliation as a new `run_inner` startup phase** (W3c1 receipts never persist `pending`; login's two-transaction shape does — the seam report names this the one gap); production provider factory: replace `UnconfiguredProviderFactory` in `DaemonDependencies::default()` with the accounts-backed factory (`resolve_for_turn` gives next-turn pickup with zero worker changes); full Anthropic model configuration release-owned; ignored live smoke (never the gate).
- **Carried from W3c1 r2 (this chunk owns them)**:
  - P3-4: graceful drain must PARK a `request_input` checkpoint (unregister without cancel; recovery reconstructs) — the ledger row's trigger is this chunk. Implement, don't ledger.
  - P3-2: pin the `cancellation_fences_start` CALL SITE with an executing test (suppress the wake, assert the fence's typed reason or factory-entries-vs-requests).
  - D3-5: provider validation whitelist unified — `DaemonDependencies` answers "creatable providers"; the rpc.rs hardcoded list dies; `"fake"` never accepted on the production wire path.
  - R9 deadlines: 15s Ping, 45s read/Pong-write deadlines (paused-time tested).

## Tests (report §6.2's list is the gate — all of it)

Two-subprocess CLI race (one winner, loser exit 75, both Ready); stale socket recovered by winner only; old/missing-feature daemon not killed; no-overlap → protocol mismatch; parent exits, daemon remains; fake validator success → next fake turn observes the alias; 401 vs 403; retryable validation/restage; descriptor-save/fsync failure + every crash boundary reconciles; identical display aliases in two profiles resolve distinct secrets; **the sentinel-secret sweep** (a unique API key is absent from events, receipts, descriptor JSON, daemon log, formatted frames/errors, and TUI/demo snapshots); ping/no-progress on paused time. Plus: park-not-cancel drain test (checkpoint survives graceful restart), the P3-2 pin, whitelist-unification test.

## Milestones (each independently green — commit at each)

- **M1 client + auto-spawn**: ResolvedProfile, RpcClient, connect/spawn/poll, skew diagnostics, the race/stale/skew/parent-exit tests.
- **M2 account machinery**: account actor, frames + goldens, stage codec, validator seam, persistence + fsync, receipt-reconciliation startup phase, 401/403/crash-boundary/alias/sentinel tests.
- **M3 integration + carried**: production factory swap, login→next-turn e2e (FakeProvider observes the alias), Ping/Pong deadlines, park-not-cancel, P3-2 pin, whitelist unification, full-gate + goldens.

## Discipline (standing law)

Tests only UP from 680 (`xtask test-count --update` per milestone; note: this worktree branch baseline). Never delete/weaken. MUTATION CHECK comments on law-bearing tests; execute the load-bearing reverts ("Verified by revert"). Additive wire only — existing goldens byte-identical. Secrets NEVER in envelopes, receipts, logs, errors — the sentinel sweep is the proof, run it per milestone. Full gate per milestone: cargo test --workspace, clippy --all-targets -- -D warnings, fmt --check, xtask test-count. Review after M3: frozen-SHA review (Fable reviewer of record while codex quota is out) gates the merge.
