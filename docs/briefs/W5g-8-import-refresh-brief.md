# W5g-8 — imported OAuth self-heals: source-first re-read, refresh fallback, no tombstones

## The live facts (probed 2026-07-31, do not re-derive)

- `https://auth.openai.com/oauth/token` refresh WORKS with our exact
  request shape (JSON, `client_id app_EMoamEEZ73f0CkXaXp7hrann`,
  `grant_type refresh_token`): 200 with `access_token`, `id_token`,
  `token_type Bearer`, `expires_in 864000`, `scope "openid profile email
  offline_access api.connectors.read api.connectors.invoke"` (superset —
  passes the guard), extra fields `earliest_refresh_at`/`oai_is` (serde
  ignores), AND a **rotated `refresh_token`** — rotation is the story:
  the imported refresh token is SHARED with the codex CLI, so whichever
  client refreshes first invalidates the other's copy.
- Consequences observed live: codex CLI refreshed on its own → haider's
  imported refresh token became `invalid_grant` → `refresh()` marked the
  account expired → `CredentialBroker::resolve`'s OAuth arm SHORT-CIRCUITS
  on `snapshot_allows_oauth` before any refresh attempt — the mark is a
  tombstone and the account never heals without a manual re-import.

## The design (three laws)

1. **Source-first self-heal.** For an IMPORTED provider (openai-oauth
   with a codex source, anthropic-oauth with a claude source), when the
   stored bundle is expired/marked-expired at resolve time, FIRST re-read
   the import source file through the EXISTING import machinery (same
   validation, same receipts, a fresh command id — the daemon already
   knows the source paths from `oauth_import` source specs, including the
   env override the tests use). If the file yields a bundle whose access
   token differs from the stored one, commit it exactly like a user-run
   `haider import` and resolve with it. This is the good-citizen path:
   codex/claude keep their own files fresh, and we never race them for
   the rotating refresh token.
2. **Own refresh as fallback.** If the source file is absent, unreadable,
   or yields the SAME stale token, fall through to the existing
   `refresh()` (it already works — the probe proved the wire). A refresh
   success must persist the ROTATED refresh token (existing bundle-commit
   path already does).
3. **Expired is a hint, not a tombstone.** `resolve`'s OAuth arm must not
   fail-fast on an expired snapshot status when laws 1/2 have a move to
   make; it fails only after both healing paths lose. Keep the single-
   flight/fence discipline — concurrent resolves must not stampede the
   source file or the token endpoint (reuse the existing flight/fence
   machinery; one heal attempt, waiters ride it).

Error taxonomy: when both paths lose, the run_failed message must name
the state plainly — `credential expired — re-run \`haider import codex\`
or sign in again` (respectively claude-code) — not a generic
provider_error. The TUI already renders run_failed reasons (W5g-6).

## Where things live

- `crates/haider-daemon/src/oauth.rs` — broker `resolve`/`resolve_oauth`
  (the short-circuit), `refresh()` (works; keep), import source specs +
  `codex_import_bundle`/`claude_import_bundle` (reuse for re-read),
  `mark_expired_if_current`.
- `crates/haider-daemon/src/accounts.rs` — the import actor path
  (`handle_oauth_import`) whose commit machinery the self-heal must ride
  (single writer law: the ACTOR commits, the broker asks it to — mirror
  how BeginOAuthRefresh round-trips through `AccountCommand`).

## Laws

- Tests NEVER inline; every law-bearing test documents its mutation +
  expected RUNTIME failure. `CARGO_INCREMENTAL=0` everywhere; finish
  with `cargo fmt --all`, workspace clippy `-D warnings` clean,
  `CARGO_INCREMENTAL=0 cargo test -p haider-daemon` (sandbox socket
  failures expected; host gate is authoritative), and
  `cargo run -p xtask -- test-count --update`.
- Do NOT touch haider-tui, haider-rpc wire shapes, Cargo.lock, versions.
- Secrets never in errors/logs/receipts. Leave changes uncommitted; no
  git commands.

## Tests (minimum)

- A resolve over a marked-expired bundle with a FRESHER source file
  commits the re-import and succeeds (mutation: keep the tombstone
  short-circuit → test fails with expired_or_replaced).
- A resolve with a stale/absent source falls back to refresh and
  succeeds against the fake token endpoint (mutation: drop the fallback
  → fails).
- Both paths losing yields the NAMED taxonomy message (mutation: generic
  error restored → message assertion fails).
- The heal is single-flight: two concurrent resolves produce ONE source
  read / ONE refresh (mutation: fence dropped → counter test fails).

Use up to 2 research subagents and 1-2 verify subagents. Print a final
summary of files changed and tests added.
