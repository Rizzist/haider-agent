# W5f-1 — `account.oauth_import`: consume stored OAuth from installed CLIs

Repo: /Users/rizzist/haider-run/haider-agent (branch `w5-f1`, already checked out for you).
You CANNOT write `.git` (sandbox): leave the tree modified and write the intended
commit message to `COMMIT_MSG.txt` at the repo root. Do not try to branch/commit.

## Goal

The owner's machine already holds subscription OAuth from two installed CLIs.
`haider` must be able to CONSUME them into its own vault as first-class
`openai-oauth` / `anthropic-oauth` accounts — same descriptors, same refresh
path, same everything as an account minted by the loopback PKCE flow. This is
authorized private testing on the owner's own accounts.

Two sources, both files the DAEMON reads itself (tokens must never transit the
client, the wire request, a receipt, or any log):

1. `codex` — `~/.codex/auth.json` (env override `HAIDER_CODEX_AUTH_PATH` for
   tests and probes). Shape: `{"OPENAI_API_KEY": ..., "tokens": {"id_token",
   "access_token", "refresh_token", "account_id"}, "last_refresh"}`. Maps to
   provider `openai-oauth`. There is no explicit expiry: best-effort-parse the
   `exp` claim out of the access-token JWT payload (base64url, NO signature
   verification — we are importing, not authenticating); if unparseable, stamp
   expiry ~15 minutes out so the existing refresh broker refreshes on first
   use. Identity (display identity / account id): best-effort claims from the
   id_token payload (`email`, `chatgpt_account_id` / the `https://api.openai.com/auth`
   claim object) — mirror whatever the existing exchange path stores in
   `OAuthIdentityV1`, leniently; a missing claim degrades to the account_id
   field or `"imported"`, never an error. The id_token itself is NOT stored
   (the bundle type has no slot for it — that is deliberate).
2. `claude-code` — `~/.claude/.credentials.json` (env override
   `HAIDER_CLAUDE_CREDS_PATH`). Shape: `{"claudeAiOauth": {"accessToken",
   "refreshToken", "expiresAt", "scopes", "subscriptionType"}}`. Maps to
   provider `anthropic-oauth`. On macOS this file usually does not exist (the
   Keychain holds it) — a missing file is an HONEST error naming the path it
   looked at, never a crash. Do NOT shell out to `security` (a daemon
   triggering a Keychain GUI prompt is a trap); file-only in this cut.

## Architecture (follow the EXISTING seams — read them first)

- The loopback OAuth flow already ends in a commit seam that: builds an
  `OAuthTokenBundleV1` (haider-accounts/src/oauth.rs), writes it to the vault
  under the profile-scoped alias key (daemon/src/profile_vault.rs —
  `scoped_vault_alias`, R10), and lands a DURABLE `account.add` descriptor
  with the revision spine, active-slot rules (first account for a provider
  becomes active, never steals), and reserved-alias fences. Find that seam in
  crates/haider-daemon/src/oauth.rs + accounts.rs and REUSE it — import is
  "the exchange already happened elsewhere". Do not invent a parallel commit
  path; if you find yourself re-implementing descriptor commit, stop and
  route through the existing one.
- The bundle you build MUST be refreshable by the EXISTING refresh broker for
  these providers: same issuer/audience/token_type/provider_id values the
  exchange path would stamp (read the provider oauth registration in the
  daemon — `sanctioned` metadata — and copy exactly; the daemon already knows
  both providers' token endpoints and client ids).
- New wire surface in haider-rpc (frame.rs):
  `RequestBody::AccountOAuthImport { command_id, source }` with
  `source: String` ("codex" | "claude-code"), and a matching
  `ResponseBody::AccountOAuthImport { descriptor, revision }` (mirror
  `AccountAdd`'s shapes). The request is durable-command-shaped (command_id)
  but the request_json/receipt must contain ONLY `{source, alias, provider}` —
  never tokens (existing law: no receipt may contain a secret). Follow the
  `account.add` receipt pattern.
- Feature flag: `FEATURE_ACCOUNT_OAUTH_IMPORT_V1 = "account_oauth_import_v1"`
  in frame.rs, advertised in the daemon's welcome feature list next to the
  other account features (find where account_oauth_pkce_v1 is advertised).
- Alias: default `openai-oauth` / `anthropic-oauth` with the smallest free
  numeric suffix on collision (the TUI uses this convention for OAuth adds —
  keep parity). Re-import onto an existing SAME-provider alias with the same
  source is allowed and REPLACES the bundle (that is the token-rotation
  use-case) — respect the oauth generation fence when replacing.
- CLI (crates/haider-cli/src/main.rs): extend the positional dispatch with
  `haider import codex|claude-code` → connect-or-spawn the profile daemon
  (reuse the `front_door`/`ensure_daemon` machinery), send the command, print
  `imported <alias> (<provider>) — <display identity>` on success, the
  daemon's error message on failure, exit 0/1. `haider import` bare lists the
  two sources and whether each file exists (path shown, no contents). Update
  the unknown-command usage string.

## Laws (workspace — violations are review kills)

- Tests NEVER inline in src files: `tests/` dirs or `*_tests.rs` via
  `#[path]` — copy the pattern next to whatever file you touch.
- Test count only moves via `cargo run -p xtask -- test-count --update`.
- `cargo clippy --workspace --all-targets -- -D warnings` must be clean.
- `CARGO_INCREMENTAL=0` on EVERY cargo invocation (disk law).
- Gate per-crate: `cargo test -p <crate>` for each crate you touched (the
  workspace-wide runner SIGABRTs on this box — never use it).
- MUTATION-CHECK LAW: every law-bearing test carries a doc comment naming its
  mutation and the expected RUNTIME failure (a compile failure is NOT a
  kill), e.g. "MUTATION CHECK: make the import path skip the reserved-alias
  fence; expected runtime failure: <test> commits over a reservation". You
  do not need to execute the mutations (the reviewer re-executes them), but
  they must be REAL: revert-able single edits with runtime-visible failures.
- Secrets: tokens go through `Zeroizing`, never into `Debug`, `format!`,
  `tracing`, receipts, or the wire. The fake tokens in tests must look fake
  ("fake-access-token-1"), never real-looking JWTs with real domains.

## Tests you must land (in the right crates' test files)

1. rpc: wire round-trip for the two new bodies + the feature constant.
2. daemon: happy codex import via `HAIDER_CODEX_AUTH_PATH` pointing at a
   tempdir fixture → descriptor committed with provider `openai-oauth`,
   active=true when first, vault holds a refreshable bundle (issuer/audience
   match the registration), revision bumped, receipt free of tokens.
3. daemon: happy claude-code import likewise (`expiresAt` honored).
4. daemon: malformed/missing file → honest error naming the path, NOTHING
   committed (descriptor store unchanged, vault unchanged).
5. daemon: reserved-alias fence — an in-flight remove/login reservation on
   the target alias fences the import (mirror the existing fence tests).
6. daemon: re-import replaces the bundle without violating the generation
   fence, and does NOT steal the active slot from another account.
7. cli: dispatch parsing (`import codex`, `import claude-code`, bare
   `import`, unknown source) — pure-function tests on whatever you factor
   the arg handling into.

## Research/verify budget

Use up to 3 research subagents to map the oauth commit seam, the receipt
pattern, and the refresh registration before writing code; use 1-3 verify
subagents before finishing to re-run the touched crates' gates and re-read
the diff against the laws above. Write a short summary of what you changed
and any deviations to `W5F1_NOTES.md` at the repo root.
