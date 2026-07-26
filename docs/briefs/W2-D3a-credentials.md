# Patch brief W2a/D3a — credential resolver (haider-accounts)

Own crates/haider-accounts (+ tests/) ONLY. Read haider-protocol (CredentialDescriptor,
CredentialAlias, RotationEvent, ErrorCode) + CONVENTIONS.md. Deps: security-framework (macOS
keychain) via workspace, serde.

Deliver:
1. Vault trait: put(alias, secret) / resolve(alias) -> SecretHandle / delete / list — with TWO
   impls: KeychainVault (macOS Security.framework, service "ai.haider.agent") and, for tests,
   MemoryVault. SecretHandle NEVER Displays/Debugs the secret (redacted Debug — test this).
2. AccountStore: descriptors (alias, provider, auth_method, identity, status, active) persisted
   via a StoreLike trait (JSON file at profile dir for now — profile_meta-adjacent; the daemon
   integration comes later); exactly-one-active-per-provider enforced; add/select/remove/list.
3. Resolver: resolve_for_provider(provider) -> (CredentialDescriptor, SecretHandle) — active
   account, status-aware (Limited{until} skipped when expired-limit passed); rotation callback
   seam: on_limited(alias, until) -> RotationDecision {RotateTo(alias), Wait, Stop} trait —
   POLICY NOT IMPLEMENTED (D3b), only the seam + a test double.
4. Env bridge: import_env(provider, ENV_VAR) helper for migration (reads env once, vaults it).
Oracle: alias round-trip; redaction; one-active invariant; resolve picks active; limited-skip;
env import; MemoryVault + real Keychain behind #[ignore] (CI has no keychain UI — document).
Gate: full workspace. Leave uncommitted.
