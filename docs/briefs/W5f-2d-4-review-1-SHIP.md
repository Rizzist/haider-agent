# W5f-2d + W5f-4 — review of record #1 — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-f2c`→main.
Authority: the owner's end-state target + the v0.0.21 installed-binary
live probe (real ChatGPT subscription, no API key) + the owner's Keychain
screenshot.

## What the live probe forced (four real blockers between "import works"
## and "a turn streams") — all fixed and verified END TO END

1. **Keychain default → FileVault (W5f-4).** Ad-hoc-signed builds made the
   macOS Keychain prompt for the login password from an app the user
   couldn't identify (owner screenshot). `FileVault` is now the default on
   EVERY platform: file-per-alias, `0700` dir / `0600` files, atomic
   temp+rename, tokens in `Zeroizing`, aliases hex-encoded (reversible, so
   `list` needs no side index). Keychain→file upgrade is graceful: a
   re-import over an orphaned descriptor whose secret vanished RESTORES it
   (prior-secret read tolerates `CredentialMissing`; add-vs-replace decided
   by the store, not the secret).
2. **Nothing triggered model discovery.** A fresh OAuth account had an
   empty catalog forever, starving both the picker and the identity
   bootstrap. The TUI now requests `provider.models_refresh` for an active
   OAuth provider with no models (once per connection, dedup set); the
   refreshed summary completes the bootstrap to the provider's REAL default
   model. New `LiveCommand::RefreshProviderModels` + `LiveReply::
   ProviderModelsRefreshed` + `ProvidersState::apply_models_refresh`.
3. **codex `/models` 400'd** — it REQUIRES a `client_version` query param.
   Added via `catalog_request_url` (extracted, statically pinned).
4. **codex `/responses` (responses-lite) contract** — rejects
   `max_output_tokens`, requires `parallel_tool_calls: false` and
   `reasoning.context: all_turns`. `responses_request_json` now branches on
   the lite flag; the API-key path is byte-for-byte unchanged.

The exact live contract is recorded in memory
(`haider-codex-subscription-api-contract`) so it is not re-discovered.

## Live acceptance (the gate no fake can pass)

`haider import codex` → identity bootstraps to `gpt-5.6-sol · openai-oauth`
(discovered, not seeded, not hardcoded) → a real turn streams `PINGACK`,
524 tokens, badge returns to IDLE, no error, **no API key, no Keychain
prompt.** This is the owner's end-state target, met.

## Mutations (reviewer-chosen, executed)

| # | Mutation | Result |
|---|---|---|
| M1 | lite payload keeps `max_output_tokens` | KILLED |
| M2 | catalog omits `client_version` | KILLED (now static, not just live) |
| M4 | FileVault skips `0700`/`0600` | KILLED |
| M5 | import migration case made fatal | KILLED |
| M6 | `boot()` drops the front-door reads | KILLED |
| M7 | bootstrap adopts without model truth | KILLED |
| + | model-refresh trigger dropped; refreshed catalog completes bootstrap | KILLED |

The M2 revert also re-taught the commit-before-mutation law: `git checkout
--` on `catalog.rs` wiped an uncommitted extraction; reconstructed and
committed FIRST, then mutated.

## Gate

Full per-crate gate `gate14.out`; workspace clippy `-D warnings` clean;
ledger 1102 → 1113.

## Honest residuals (non-blocking)

- Anthropic subscription (`claude-code` import, `/v1/models`, Claude Max
  turns) is UNVERIFIED live — macOS keeps those creds in the Keychain,
  which the daemon deliberately won't prompt-read, and no `.credentials.
  json` was present. The code paths mirror the OpenAI ones but the
  Anthropic `/v1/models` status and any Claude responses contract are not
  live-confirmed. Flagged for the owner's Claude Max account.
- `context_window` still comes from seeds/defaults (display-only now the
  output cap is decoupled); catalog-driven windows are future work.

## Verdict

**SHIP as v0.0.22** (re-tag: the prior v0.0.22 CI failed on a stale
Cargo.lock, now regenerated).
