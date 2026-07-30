# W5c.2b — review of record #1 — SHIP (after reviewer fixes)

Reviewer: Fable 5 (codex implements, Fable reviews — owner rule reaffirmed
2026-07-30). Branch `w5-c2b`: implementation `d32ce8f` (28 files,
+4377/-263), reviewer fixes `879709c` + pins commit. Design authority:
`docs/research/w5-provider-research-report.md` §4.1, §4.2, R5, R6.

## Verdict per binding criterion

1. **`account.set_active` — PASS.** Preflight replay → durable recovery
   coordinates (provider + prior alias) in the receipt → claim → select →
   resolver snapshot → finalize (receipt + revision, one transaction) →
   management publish. The server derives the provider from the alias; a
   client-supplied provider is never trusted. Startup reconciliation resumes
   a pending select from its recovery coordinates.

2. **`account.remove` crash order (§4.2) — PASS.** Claim + durable alias
   reservation → fence invalidation (late refresh completions go stale even
   across re-add) → descriptor removal → resolver publish → vault delete →
   finalize + reservation release → management publish. Vault deletion
   strictly FOLLOWS descriptor removal, so no crash point leaves an active
   descriptor pointing at a deleted secret — an orphan secret is the worst
   case, exactly as specified. Startup: `reconcile_remove_receipts` runs
   FIRST, retries the idempotent deletion, keeps the pending receipt +
   durable reservation when the vault refuses, and Ready proceeds with
   add/login fenced. The response names the store's deterministic successor.

3. **`account.set_default_model` / `provider.configure` — PASS.**
   Registered-model invariants enforced; built-in/existing identity-field
   changes rejected; endpoint validation reuses the W5a origin guard
   (no second validator): remote plain-HTTP and redirecting endpoints fail
   with actionable reasons.

4. **Expected-revision CAS — PASS.** Replay is checked BEFORE the revision
   comparison inside the claim transaction itself (not merely at actor
   preflight), so a committed command stays idempotent after later revisions;
   a genuinely new stale mutation gets `revision_conflict` retryable with
   bounded details.

5. **R7 — PASS.** All four mutations are owned jobs `try_send`-ed to the one
   account actor; no connection task awaits vault/probe/validation.

6. **Carried P3s — CLOSED.** `provider_configure_v1` advertised as its own
   feature; the hardcoded registry is replaced by `ProviderRegistry` and the
   factory path renders unknown providers unavailable/disabled.

## Findings

- **[P1] R10 destroyed: global alias became the raw machine-global Keychain
  key. FIXED IN THIS REVIEW (`879709c`).** The patch deleted the
  "aliases in different profiles can never collide in the Keychain (R10)"
  law, demoted `physical_alias` to `#[cfg(test)]`, and used the user's global
  alias as the Keychain account key under the one machine-global service.
  Failure scenario: profile A and profile B both add alias `work` — B's login
  clobbers A's secret while A's descriptor still points at it, and B's
  `account.remove` deletes the item A still references. Fix: `ProfileVault`
  wraps the platform vault at the single `AccountsRuntime::initialize` seam —
  writes go to `{blake3(profile)[..16]}::{alias}` (`haider-vault-key-v1`),
  reads fall back to the raw key for pre-scoping items (legacy physical
  aliases were already profile-hash-namespaced, so the fallback cannot cross
  profiles), deletes clear both keys. Descriptor semantics unchanged. Four
  pins, two mutation-checked (identity mapping, fallback removal) — both
  KILLED.
- **[P2] Stale integration tests pinned the pre-W5c.2b semantics. FIXED.**
  `staged_login_commits_…` failed deterministically on a real socket
  (`identity == "work"`, `alias starts_with "anthropic-"`) — codex's sandbox
  denied every UDS bind, so it shipped without ever running them. Updated to
  the new contract (alias = global alias, identity = validator identity,
  vault item under the scoped key, raw key must NOT resolve), plus the three
  OAuth tests and one retry test that read the vault by raw key.
- **[P2] Reserved-alias fence was unpinned. FIXED.** Disabling both fences
  (`handle_login` + `handle_oauth_add`) left the entire daemon suite green.
  A login could re-occupy an alias whose removal cleanup was still pending;
  the retried vault deletion would then destroy the NEW credential's secret.
  Pin: `reserved_alias_fences_login_and_oauth_add_until_remove_finalizes`
  (busy + retryable + nothing persisted). Mutation re-executed: KILLED.
- **[P2] Unknown-api-family availability conjunct was unpinned. FIXED.**
  `available = enabled && api_family != Unknown` weakened to `enabled` alone
  survived the whole suite — codex's factory pin only covers the
  `enabled=false` creation path, not the tolerant-decode case where a NEWER
  daemon's enabled profile round-trips through this build as `Unknown`.
  Pin: `enabled_profile_with_unknown_api_family_is_never_available`.
  Mutation re-executed: KILLED.

## Audit integrity — mutations re-executed by the reviewer

| # | Mutation | Result |
|---|---|---|
| C1 | Hoist the expected-revision CAS above the committed-replay lookup | KILLED |
| C2 | Retain the alias reservation after remove finalization | KILLED (after correcting the mutation to the real `command_id`-keyed DELETE) |
| C3 | Disable both reserved-alias fences | SURVIVED → pin added → KILLED |
| C4 | Weaken availability to `enabled` alone | SURVIVED → pin added → KILLED |
| F1 | `ProfileVault::scoped` → identity mapping | KILLED |
| F2 | Drop the legacy raw-key fallback | KILLED |

## Gate (reviewer-run, per-crate; codex's socket failures were its sandbox)

clippy `--workspace --all-targets -D warnings` clean. Ledger 1014 → 1031.
All 13 crates green including daemond (the gate's one pre-fix failure was the
stale-semantics witness above).

## Verdict

**SHIP** as of the reviewer-fix commits. The mutation architecture
(receipts + recovery coordinates + single-transaction finalize + durable
reservations) is genuinely crash-ordered; the one P1 was a lost invariant at
the vault boundary, restored without disturbing the new alias semantics.
