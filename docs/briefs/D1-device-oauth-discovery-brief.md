# D1 — device OAuth discovery + account refresh actions

Owner directive (2026-08-04, Diff Forge ADE as reference): the harness
automatically identifies OAuth credentials already on the device and
offers import; per-account refresh/add actions. NO haider-tui (D2).

## Scope

1. **Device credential discovery** (daemon-owned, read-only, safe):
   probe the known first-party credential stores —
   `~/.codex/auth.json` (openai; the existing `haider import codex`
   parser is the authority), `~/.claude/.credentials.json` +
   `~/.claude/oauth` variants (anthropic Claude Code — research the
   real current paths/shape in-lane), `~/.kimi/credentials/
   kimi-code.json` (kimi-code OAuth bundle — shape documented in
   docs/research/b6k-kimi-oauth-research.md), `~/.gemini/
   oauth_creds.json` (gemini CLI — research shape). Discovery reads
   METADATA ONLY into the report (provider, account label/email if
   present, expiry-ish freshness, path) — never token bytes; bounded
   reads; malformed/absent → skipped silently.
2. **Wire**: additive `account.device_candidates` RPC returning the
   discovered list (provider, source label, freshness hint, an opaque
   candidate id) + feature bit. Secrets NEVER ride this response.
3. **Import per candidate**: `account.import_device { candidate }` —
   receipted (R2), routes through the per-provider import machinery
   (codex import exists; ADD kimi-code import using the B6k bundle
   shape incl. the rotating refresh token + device id; claude-code +
   gemini imports if their shapes are tractable in-lane, else
   candidate reported with `import_supported:false` and an honest
   reason — never guess a parser).
4. **Refresh-now**: additive `account.refresh { alias }` — forces the
   broker's refresh path for that alias immediately (reusing the
   existing serialized machinery: rotation-safe, lease-held, typed
   errors incl. relogin-required). Receipted command.
5. Vault/no-leak laws throughout; discovery paths configurable off via
   an env/profile switch (`discovery_disabled` honest state).

## Laws (minimum)

- discovery_reports_metadata_never_token_bytes (fixture stores with
  real-shaped bundles; assert response bytes contain no token
  material).
- absent_or_malformed_stores_are_skipped_silently.
- import_device_is_receipted_and_lands_a_working_account (codex +
  kimi fixtures).
- unsupported_candidate_is_honest_not_guessed.
- refresh_now_rides_the_serialized_lease_and_rotates_safely
  (concurrent refresh_now + resolve — the B6k concurrency shape).
- refresh_now_expired_terminal_names_relogin_typed.
- goldens additive + tolerance re-proved.

Standing lane laws: tests never inline; mutation-notes with RUNTIME
kills; CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
ledger; NO haider-tui; no Cargo.lock; no version bumps; leave
uncommitted; no git. Up to 3 research + 2 verify subagents.
