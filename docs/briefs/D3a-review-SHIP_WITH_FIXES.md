codex
Reviewed exact worktree `w2-d3a` at `45cfb0f`; tree remains clean.

### Findings

1. **Medium — Keychain retryability mapping needs correction.** [`put`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/keychain.rs:47>) and idempotent [`delete`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/keychain.rs:68>) mark every failure non-retryable, while resolve/list mark every non-missing failure retryable—including permanent errors. Classify by OSStatus, with tests; transient failures of create-or-update `put` should be retryable, while parameter/user-cancel failures should not.

2. **Medium — Security oracles are incomplete.** The fixed [`NotUnicode` arm](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/env_bridge.rs:39>) has no regression test. Do not waive it: use a Unix subprocess with `Command::env` and `OsStringExt::from_vec`, requiring no unsafe code, and assert the sentinel is absent from message/Debug/Serialize/captured output. Also add a currently-limited rotation target test proving callback count remains one, and replace the vacuous [`!json.contains("secret")`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/tests/accounts_tests.rs:289>) check with a unique sentinel actually stored in the vault.

Audit conclusions:

- No secret-escape code path found: `SecretHandle` is crate-minted, non-cloneable/non-serializable, and redacts Debug/Display; `MemoryVault` exposes no Debug; errors contain aliases or OSStatus text only; `accounts.json` serializes descriptors without secret fields. The Keychain dependency’s error display derives solely from OSStatus.
- One-active-per-provider is correctly enforced on load and mutation. Commit ordering is exactly revalidate → atomic file save → in-memory swap. Old and new snapshots are both valid across process crashes.
- Rotation implementation is exactly-once and single-hop in code.
- `NotUnicode → CredentialMissing` is accepted as-is: caller arguments are valid, but no importable credential exists; `InvalidArgument` would misleadingly blame the API arguments. Non-retryable is appropriate.
- Gates: workspace tests passed; 86-test workspace guard passed; formatting passed. Real Keychain test is correctly ignored/documented. Clippy could not reopen Cargo’s lock under the read-only review sandbox.

VERDICT: SHIP_WITH_FIXES
hook: Stop
hook: Stop Completed
tokens used
108,264
Reviewed exact worktree `w2-d3a` at `45cfb0f`; tree remains clean.

### Findings

1. **Medium — Keychain retryability mapping needs correction.** [`put`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/keychain.rs:47>) and idempotent [`delete`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/keychain.rs:68>) mark every failure non-retryable, while resolve/list mark every non-missing failure retryable—including permanent errors. Classify by OSStatus, with tests; transient failures of create-or-update `put` should be retryable, while parameter/user-cancel failures should not.

2. **Medium — Security oracles are incomplete.** The fixed [`NotUnicode` arm](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/src/env_bridge.rs:39>) has no regression test. Do not waive it: use a Unix subprocess with `Command::env` and `OsStringExt::from_vec`, requiring no unsafe code, and assert the sentinel is absent from message/Debug/Serialize/captured output. Also add a currently-limited rotation target test proving callback count remains one, and replace the vacuous [`!json.contains("secret")`](</Users/rizzist/Documents/CODING/haider-agent-d3a/crates/haider-accounts/tests/accounts_tests.rs:289>) check with a unique sentinel actually stored in the vault.

Audit conclusions:

- No secret-escape code path found: `SecretHandle` is crate-minted, non-cloneable/non-serializable, and redacts Debug/Display; `MemoryVault` exposes no Debug; errors contain aliases or OSStatus text only; `accounts.json` serializes descriptors without secret fields. The Keychain dependency’s error display derives solely from OSStatus.
- One-active-per-provider is correctly enforced on load and mutation. Commit ordering is exactly revalidate → atomic file save → in-memory swap. Old and new snapshots are both valid across process crashes.
- Rotation implementation is exactly-once and single-hop in code.
- `NotUnicode → CredentialMissing` is accepted as-is: caller arguments are valid, but no importable credential exists; `InvalidArgument` would misleadingly blame the API arguments. Non-retryable is appropriate.
- Gates: workspace tests passed; 86-test workspace guard passed; formatting passed. Real Keychain test is correctly ignored/documented. Clippy could not reopen Cargo’s lock under the read-only review sandbox.

VERDICT: SHIP_WITH_FIXES

