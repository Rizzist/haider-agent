# B6k — Kimi (Moonshot) OAuth provider

AUTHORITY: docs/research/b6k-kimi-oauth-research.md (read WHOLE,
first). Owner-requested wave. NO haider-tui here (B6b adds buttons).

## Scope

1. **Device-flow profile** on the existing OAuth machinery: RFC 8628
   form-encoded against auth.kimi.com per the research (public
   client_id, no PKCE/secret), six X-Msh-* headers with a
   vault-persisted device UUID, poll/backoff per spec errors. Reuse
   the codex-flow structural pattern; bounded response reads;
   secrets only ever as SecretHandle; no token in any log/journal.
2. **Rotating-refresh serialization (THE risk)**: refresh at turn
   boundary when remaining < max(300s, expires_in/2), force on 401 +
   ONE retry after re-reading the vault (another process may have
   rotated); atomic vault persist BEFORE first use; rejected-refresh
   tombstone (300s) → typed re-login error. Pin with a two-refresher
   concurrency law test.
3. **Provider wiring**: "kimi-oauth" builtin → EXISTING
   OpenAI-compatible chat-completions adapter, base
   https://api.kimi.com/coding/v1, Authorization: Bearer. Adapter
   tolerances (additive, gated to this provider): max_completion_tokens
   instead of max_tokens; extra_body.thinking passthrough seam
   (default off). CapabilityDoc honest (vision per /models flags).
4. **Catalog**: source for the nonstandard GET /coding/v1/models
   (Bearer auth mode), parse id/context_length/display_name/protocol/
   thinking flags; context_length → context_window;
   protocol:"anthropic" models are EXCLUDED from the published set for
   now (documented residual). Existing catalog sources byte-identical
   (golden).
5. **Validator**: 1-token ping through the real adapter path.
   Feature/registry/goldens additive; older-client tolerance re-proved.

## Laws (minimum)

- device_flow_polls_to_tokens_with_required_msh_headers (fixture).
- concurrent_refreshers_never_destroy_the_rotated_token (the flock/
  serialize law — two tasks race one rotation; both end authorized).
- rejected_refresh_tombstones_and_surfaces_typed_relogin.
- kimi_requests_use_bearer_and_max_completion_tokens (payload golden).
- models_catalog_parses_context_length_and_skips_anthropic_protocol.
- no_secret_bytes_in_errors_journal_or_logs (existing no-leak pattern).
- existing_catalog_and_wire_goldens_byte_identical.

Standing lane laws: tests never inline; mutation-notes doc with
RUNTIME failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D
warnings; additive protocol only; ledger update; NO haider-tui; no
Cargo.lock; no version bumps; leave changes uncommitted; run no git
commands. Use up to 3 research subagents and 2 verify subagents.
Finish with a summary of files changed and tests added.
