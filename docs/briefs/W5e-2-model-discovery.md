# W5e-2 — model discovery from the providers' OWN sources (→ v0.0.18)

Owner requirement (2026-07-30): model choice must come "from the openai
codex/claude code CLIs directly, not hardcoded". This brief records what the
installed CLIs actually do, established by inspection on 2026-07-30, and the
design that follows.

## What codex does (CONFIRMED from the installed 0.146.0 build)

Evidence: `~/.codex/models_cache.json` (live, refreshed 2026-07-30T08:02:48Z)
plus binary symbols `codex-api/src/endpoint/models.rs`,
`app-server/src/models_refresh_worker.rs`, `models-manager/src/manager.rs`,
and the base constant `https://chatgpt.com/backend-api/codex`.

- **Endpoint**: `GET https://chatgpt.com/backend-api/codex/models`,
  authorized with the ChatGPT subscription OAuth access token — the same
  token class W5b already stores and refreshes.
- **Conditional fetch**: `etag` (`W/"…"`) + `client_version` are persisted
  and replayed; a 304 keeps the cache.
- **Cache**: `models_cache.json` with `fetched_at`, staleness evaluation, and
  the manager's three modes — `online`, `offline`, `online_if_uncached`
  (log strings: "cache hit", "cache is stale", "cache version mismatch",
  "cache miss, fetching remote models", "using cached models for
  OnlineIfUncached").
- **Per-model payload** (richer than a slug list):
  `slug`, `display_name`, `description`, `default_reasoning_level`,
  `supported_reasoning_levels[{effort, description}]` (low → ultra),
  `visibility` (`list` gates what a picker shows), `supported_in_api`,
  `priority` (picker ordering), `additional_speed_tiers`, `service_tiers`,
  `base_instructions`.

## What Claude Code does

The darwin-arm64 `claude` binary is a packed 59 MB single-file bundle — no
readable strings, and `~/.claude` holds NO models cache (only changelog and
issue caches). So its list is fetched live rather than cached to disk.

Design decision: use the documented Anthropic models endpoint with the
credentials we already prove work for inference —
`GET {ANTHROPIC_OAUTH_BASE_URL}/v1/models` with the OAuth bearer plus the
same beta header set W5b.2 already sends (`AnthropicOAuthBeta`). This is
discovered-not-hardcoded and reuses a working auth path.

**Honest risk**: if `/v1/models` is not served for subscription OAuth
tokens (403/404), we do NOT invent a list. The fallback ladder is
last-known-good cache → the models the account has been observed to accept
→ an explicit "model list unavailable; type a model id" state in the picker.
Never a hardcoded slug table pretending to be discovery. Verify against a
real Claude Max token during implementation and record the observed status
code in the review.

## Design

1. **`ModelCatalog` in haider-provider**: per-provider discovery trait
   (`discover(&self, credential) -> Vec<DiscoveredModel>`), with the
   OpenAI-subscription and Anthropic-OAuth implementations. Reuses the W5a
   `FixedOriginGuard`/SSRF discipline — these are key-bearing requests to
   fixed origins, so resolve-validate-pin + `.no_proxy()` + no-redirect
   apply exactly as they do to the token endpoints.
2. **`DiscoveredModel`**: `slug`, `display_name`, `description`,
   `default_effort`, `supported_efforts`, `visible`, `priority`. Effort
   levels are the codex payload's; Anthropic's absent field means `None`,
   not a fabricated ladder.
3. **Durable cache**: schema v7 `provider_models` (provider, slug, payload
   JSON, `etag`, `fetched_at_ms`) written through the account actor (single
   writer, R6) so a restart serves last-known instantly and a refresh is
   conditional. TTL + `online_if_uncached` semantics mirrored from codex.
4. **`provider.models_refresh` RPC** (durable-ish read job): R7 hand-off —
   the connection task only `try_send`s; discovery HTTP runs in an owned
   task; the result publishes a new management revision so `/providers` and
   `/model` pick it up through the existing revision gate.
5. **Registry becomes discovered**: `provider_management_summaries`' model
   inventory reads the cache instead of the literals; this CLOSES the
   W5c.2a carried P3 (hardcoded `gpt-5.6` lists). A provider with no cache
   entry reports its models as unknown — never healthy-with-guesses.

## Test law

- Discovery is a FAKE-server test (like the OAuth engine's fake AS): serve
  a captured codex payload shape, assert the parse, the ETag replay, the
  304-keeps-cache path, and the stale/offline mode matrix.
- SSRF pins: a discovery request to a private/link-local/metadata origin
  must be refused before any token leaves — reuse the W5a/W5b.2 pin shape.
- MUTATION-CHECK: dropping the `visibility` filter must surface a
  non-listed model in the picker; dropping the ETag must re-download;
  dropping the origin guard must fail the SSRF pin.
- NO test may assert a hardcoded model slug as "the" list — that would
  re-introduce exactly what this wave removes. Assert SHAPE and PROVENANCE.
