# W10a — provider.remove: durable custom-provider removal

Final daemon wave before the owner's stop line. Scope is ONE addition —
do not refactor neighbors.

## Scope (rpc/daemon/store — NO haider-tui)

`provider.remove { command_id, provider, expected_revision }`:
- CUSTOM providers only — a builtin/factory provider name refuses with a
  typed reason (the registry is release-owned).
- Refuses while any account (credential descriptor) still references the
  provider — typed error NAMING the blocking aliases; the owner removes
  accounts first (no cascade — removal must never silently destroy
  credentials).
- Receipt-backed like every R2 mutation (same command_id replays the
  committed response; changed body under the same id rejected), durable,
  revision-fenced (expected_revision mismatch → typed conflict), and the
  provider's discovered-model cache/config rows are removed so a daemon
  restart does NOT resurrect the profile (the W5c.2b reconciliation must
  treat the removal receipt as authoritative — mirror how account.remove
  beats sqlite resurrection).
- Response carries the new registry revision. Welcome advertises
  `provider_remove_v1`.
- Wire goldens updated additively; protocol frozen shapes reused.

## Laws

Standing lane laws (tests never inline; mutation docs with RUNTIME
failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
additive protocol; goldens; ledger update; no haider-tui; no Cargo.lock;
no versions; leave uncommitted; no git). Sandbox socket failures
expected — host gate authoritative.

## Tests (minimum)

- Remove commits, list no longer shows the provider, restart does not
  resurrect it (mutation: skip the durable row removal → resurrection
  test fails).
- Builtin refusal + blocking-accounts refusal with alias names
  (mutations: drop either guard → fails).
- Receipt replay + changed-body rejection + revision-fence conflict
  (mutations per R2 law).
- Feature advertisement (mutation: drop the feature → discovery test
  fails).

Use up to 2 research subagents and 1 verify subagent. Print a final
summary of files changed and tests added.
