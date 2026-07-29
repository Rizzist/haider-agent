# W5c.1 — review of record #1 — SHIP (with one fix applied)

Reviewer: Fable 5 (owner-mandated: every review/verify pass is Fable 5 itself;
codex implements, it does not review). Branch `w5-c1`, frozen at `58d399f`,
diff `12fe8ab..58d399f` (23 files, +1585/-119). Design authority:
`docs/research/w5-provider-research-report.md` §3.5, §4.3, §6, R4, R8.

Scope: turn credential resolution/refresh/rotation wired to the merged
`Resolver` + single-flight `CredentialBroker`. Prior state (report §1.3):
`AccountsProviderFactory` bypassed `Resolver`, resolved the active descriptor
straight from the snapshot, and pinned it for the whole turn.

## Verdict per binding criterion

1. **Resolver actually invoked (§4.3) — PASS.** Both factory paths construct a
   real `Resolver`. The broker path routes through `AccountCommand::
   ResolveCredential` into `resolve_account` (`accounts.rs:764`), which calls
   `resolve_descriptor_for_provider` / `resolve_alternate_descriptor`. The
   broker-less path builds an `AccountStore` over `ReadOnlySnapshotStore` and
   resolves through the same checked seam. The direct-snapshot `find(provider
   && active)` bypass is gone. Splitting the resolver into descriptor-only
   halves (`resolver.rs:98`, `:161`) is the right shape: descriptor selection
   is synchronous inside the actor, and vault/refresh I/O happens only after
   the actor releases descriptor ownership.

2. **Rotation is ONCE and pre-first-event ONLY (§6/R8) — PASS.** Two call
   sites into `prepare_pre_first_event_retry` (`actor.rs:1306`): stream-open
   failure, and stream error guarded by `!provider_event_seen`.
   `provider_event_seen` is per provider request (`actor.rs:889`);
   `rotation_budget_consumed` is declared outside the `'requests` loop
   (`actor.rs:737`), so the one hop is turn-wide, not request-wide. A
   factory-time alternate arrives pre-consumed via `rotation_budget_consumed`,
   and its `RotationEvent` is committed before the first provider call.
   Cross-request rotation inside one turn is intended, which is why usage
   gained per-account subtotals (`AccountUsage`); the merge in
   `cumulative_usage` is correct and the legacy `account` field collapses to
   `Some` only when exactly one account contributed.

3. **Policy invoked exactly once; no fabricated deadline (§3.5/R4) — PASS.**
   All three triggers funnel through the one `resolve_alternate_descriptor`
   helper (same-provider + usable-now + single hop). `AuthExpired` /
   `RefreshFailed` durably set `CredentialStatus::Expired` — not a
   `Limited{until_ms}` lie — and `RotationTrigger::cause()` maps both to
   `RotationCause::Error`. Self-rotation is structurally blocked: the
   triggering alias' status is written before the policy reads `accounts
   .list()`, so it can no longer be selected. `Wait` consumes the hop, which
   is what "policy invoked once per logical turn" requires.

4. **Refresh race safety (§3.5) — PASS.** `resolve_oauth` gained a
   `force_refresh` flag that reuses the existing `flights` single-flight map
   and generation `fences` unchanged; the `RefreshKey` is force-agnostic, so
   concurrent forced and unforced refreshes coalesce, and every waiter
   re-reads the vault after `flight.wait()`. The second oauth.rs hunk is a
   genuine correctness fix: a *forced* refresh may no longer soft-succeed into
   `Ok(())` on a retryable error with a still-unexpired bundle, which would
   otherwise hand back the same token that just produced the 401. No
   actor/store lock is held across HTTP — `resolve_account` is fully
   synchronous inside the actor, refresh runs in a spawned task, and the
   `flights`/`fences` std mutexes are released before any await. No
   broker↔actor cycle exists (the actor never calls the broker), so no
   deadlock.

5. **R7 + no regression — PASS.** Connection routing is untouched.
   `drive_error_outcome_with_items(DriveError::Provider(e))` and
   `provider_failure_outcome_with_items(e)` both reduce to
   `errored_outcome_with_items(provider_error_to_haider(e))`, so widening the
   error arm changed no observable failure behavior. Retry budget accounting
   is unchanged (`provider_request_count` still increments only at
   `provider_attempt == 0`). W5a/W5b/W5b.2 properties are untouched.

## Audit integrity — mutations re-executed by the reviewer

codex's "Verified by revert" claims were not taken on trust. Five load-bearing
mutations re-executed independently; **all five killed at runtime** (no
compile-failure passes):

| # | Mutation | Result |
|---|---|---|
| M1 | `Err(error) if !provider_event_seen` → `if true` (allow post-delta rotation) | KILLED |
| M2 | drop `*rotation_budget_consumed = true` in the `Rotate` arm (allow two hops) | KILLED |
| M3 | `AuthExpired\|RefreshFailed` → `Limited{now+60s}` (fabricated deadline) | KILLED |
| M4 | replace `resolve_descriptor_for_provider` with the direct active descriptor | KILLED |
| M5 | drop `accounts.select(&selected.alias)?` (non-durable rotation) | KILLED |

`rotation_is_once_pre_first_event_and_durable_before_the_alternate` is not
vacuous: it drives the real `HarnessActor` against a real `MemoryStore` and
asserts on committed `EventPayload::Rotation` events and on a provider that
inspects the store to prove the rotation was committed *before* it was called.

## Findings

- **[P2] `accounts.rs:2104` — retryable rotation bookkeeping killed the turn.
  FIXED IN THIS REVIEW.** `AccountsAttemptResolver::resolve` propagated any
  non-`CredentialLimited`/`Unauthorized` resolver error as `Err`, which core
  turns into `DriveError::Account` → `Errored`. Failure scenario: a provider
  rate-limit arrives at request open, rotation is attempted, and
  `accounts.set_status()` fails with a transient retryable store error
  (`StoreLocked`). Before W5c.1 that rate limit backed off and retried; after
  it, the turn dies on an error the provider itself said to retry — rotation
  bookkeeping became *more* fatal than the failure it was optimizing. Fixed by
  routing any `error.retryable` resolver failure to
  `ProviderAttemptDecision::Wait`. New pin:
  `retryable_rotation_bookkeeping_failure_waits_instead_of_killing_the_turn`
  (mutation-checked: dropping the `error.retryable` arm makes `StoreLocked`
  escape as `Err` — KILLED).
- **[P3] `accounts_tests.rs:191` — test name over-claims durability.**
  `..._durably_selects_...` asserts on the in-process snapshot and a
  memory-backed `AccountStore`; no descriptor is ever reloaded from the
  on-disk store. The persistence *path* is exercised, the disk round-trip is
  not. Non-blocking.
- **[P3] `accounts.rs:764` — failure-branch resolver invocation is unpinned.**
  M4 kills only through the initial-resolution path. A mutation that bypasses
  `resolve_alternate_descriptor` in the *failure* branch alone survives,
  because every alternate in the fixture is valid. The code is correct; the
  coverage is asymmetric. Non-blocking.
- **[P3] `accounts.rs:1923` — broker-less path can publish an undurable
  rotation.** It computes a `RotationEvent` core will commit but never calls
  `select()`, and `ReadOnlySnapshotStore::save` refuses writes anyway, so the
  same rotation would re-fire every turn. Unreachable today: `broker == None`
  ⟺ `VaultProvision::Unsupported`, and the vault guard returns first. Latent
  hazard if the broker ever becomes optional while a vault is present.

## Gate (reviewer-run, per-crate; `cargo test --workspace` SIGABRTs on this box)

clippy `--workspace --all-targets -D warnings` clean. Test ledger 1002 → 1003.

protocol 23 · accounts 20 · core 41 · provider 47 · **daemon 143** · daemond 86
· rpc 45 · tui 465 · cli 21 · store 35 · tools 69 · client 18 · verify 1 — all
0 failed. haider-daemon all-targets passed this run, confirming the earlier
abort was environmental resource exhaustion, not a defect.

## Verdict

**SHIP.** Resolver genuinely invoked, rotation once and pre-first-event only,
single policy invocation with no fabricated deadline, refresh race-safe, no
regression. The one P2 is fixed and pinned in this review; the three P3s are
coverage/latent-hazard notes carried forward, none blocking.
