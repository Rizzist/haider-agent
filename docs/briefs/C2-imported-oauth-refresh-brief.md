# C2 — imported openai-oauth broker refresh (closes W5g-6)

The gap has bitten live twice: `haider import codex` copies tokens at
import time and the daemon never refreshes them, so turns die exit-70
("OAuth authorization expired") once the access token ages out, and
the only remedy is re-running the import. B6k (Kimi) built the full
broker-side rotating-refresh machinery — port that discipline to the
imported openai-oauth path. NO haider-tui.

## Scope (haider-daemon oauth.rs + accounts as needed)

1. On resolve, when an imported openai-oauth bundle is expired or
   within the refresh threshold, the broker refreshes it against the
   codex token endpoint using the stored refresh token (the import
   already vaults it — verify; if absent, import must start storing it
   additively) instead of surfacing terminal expiry.
2. Reuse the B6k discipline wholesale: vault-alias lease serialization,
   persist-before-use, re-read-on-401 adoption (another process may
   have refreshed), bounded retries for 429/5xx, terminal
   invalid_grant → typed re-login/re-import error naming the remedy,
   rejected-refresh tombstone. Check whether codex refresh responses
   ROTATE the refresh token; handle both rotate and non-rotate shapes.
3. The existing one-use codex fallback refresh law
   (codex_fallback_refresh_is_one_use_and_import_scoped) must be
   reconciled explicitly: extend or supersede it with a documented
   decision — never silently violate a pinned law.
4. No-leak law throughout; secrets only as SecretHandle; bounded
   response reads (bounded_response precedent).

## Laws (minimum)

- expired_imported_bundle_refreshes_instead_of_terminal_exit70.
- concurrent_imported_refreshers_adopt_not_destroy (port of the B6k
  concurrency law; non-degenerate fixture).
- terminal_invalid_grant_names_reimport_remedy_typed.
- refresh_never_replays_a_rotated_token (if rotation observed).
- no_secret_bytes_in_errors_journal_or_logs.
- existing openai-oauth login/pkce flows byte-identical (goldens/suites).

Standing lane laws: tests never inline; mutation-notes with RUNTIME
failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings;
additive only; ledger; NO haider-tui; no Cargo.lock; no version bumps;
leave uncommitted; no git. Up to 3 research + 2 verify subagents.
