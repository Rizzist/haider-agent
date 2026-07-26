# Patch brief W2a/D3a.1 — SHIP_WITH_FIXES completion (2 mediums)

Worktree /Users/rizzist/Documents/CODING/haider-agent-d3a, branch w2-d3a. Full findings:
docs/briefs/D3a-review-SHIP_WITH_FIXES.md. Apply both:
1. Keychain retryability by OSStatus classification (crates/haider-accounts/src/keychain.rs):
   transient (errSecInteractionNotAllowed/locked/auth-UI, IO) → retryable for put AND resolve;
   permanent (errSecParam, user-cancel, missing-entitlement) → non-retryable everywhere;
   idempotent delete of missing stays success. Unit-test the mapping fn directly (pure
   OSStatus → (code, retryable) table).
2. Oracle strengthening (tests/accounts_tests.rs): (a) NotUnicode subprocess regression test —
   spawn a helper via Command::env(OsString::from_vec(vec![0x66,0x6f,0xff])) running the
   import path (a #[test]-gated bin or the test binary re-invoked with an env flag), assert a
   unique sentinel byte-sequence absent from message/Debug/serialized error/captured output;
   (b) rotation-target-also-limited test: callback invoked EXACTLY once, typed error, no
   second hop; (c) replace the vacuous secret-scan with a unique sentinel actually vaulted
   then asserted absent from accounts.json bytes.
Gate: cargo test -p haider-accounts, clippy -D warnings, fmt, xtask test-count --update.
Leave uncommitted (amends onto the frozen commit at review time).
