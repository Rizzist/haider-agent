# W5f-1 — OAuth CLI credential import

## Summary

- Added the `account.oauth_import` RPC request/response and
  `account_oauth_import_v1` welcome feature.
- Added daemon-local, file-only importers for Codex and Claude Code. Token
  material is parsed through zeroizing buffers and never enters the request,
  receipt, descriptor, debug output, or logs. An unparseable Codex JWT expiry
  gets a durable one-use vault marker so the broker refreshes it on first use
  without changing normal PKCE refresh timing.
- Routed imported bundles through the same OAuth vault/descriptor/revision
  commit seam as loopback PKCE accounts, including first-account activation,
  reserved-alias fences, durable receipts, re-import replacement, and refresh
  generation fencing.
- Added `haider import`, including source discovery for bare `import` and
  connect-or-spawn imports for `codex` and `claude-code`.
- Added RPC, daemon, broker, remote-gate, alias-incarnation, and CLI dispatch
  regression tests. The test ledger is updated from 1084 to 1097.

## Verification

- `CARGO_INCREMENTAL=0 cargo fmt --all -- --check`
- `CARGO_INCREMENTAL=0 cargo test -p haider-accounts`
- `CARGO_INCREMENTAL=0 cargo test -p haider-rpc`
- `CARGO_INCREMENTAL=0 cargo test -p haider-cli`
- `CARGO_INCREMENTAL=0 cargo test -p haider-daemon`
- `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings`
- `CARGO_INCREMENTAL=0 cargo run -p xtask -- test-count --update`

All gates passed. The existing live Anthropic smoke test remained ignored as
designed.

## Deviations

None. The requested macOS behavior remains file-only; the daemon does not
invoke `security` or trigger Keychain UI.
