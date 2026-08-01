# B6k research — Kimi (Moonshot) OAuth provider

Fable web research 2026-08-01, verified against MoonshotAI/kimi-cli
(Python) and MoonshotAI/kimi-code (TS) source + official Kimi Code docs.

VERDICT: OAuth YES — first-party-reusable (Codex-style; no third-party
registration exists, ecosystem reuses the public client_id unblocked;
console API keys are the official third-party path and hit the SAME
endpoints).

## Device flow (RFC 8628, form-encoded; NO PKCE, NO secret)

- Host https://auth.kimi.com ; client_id
  17e5f671-d194-4dfb-9706-5516cb48c098 (hardcoded in both first-party
  clients).
- POST /api/oauth/device_authorization {client_id} → user_code,
  device_code, verification_uri_complete, expires_in, interval(5s).
- POST /api/oauth/token grant_type=
  urn:ietf:params:oauth:grant-type:device_code (poll;
  authorization_pending/slow_down/expired_token/access_denied) →
  {access_token, refresh_token, expires_in, scope, token_type}.
- Refresh: same endpoint, grant_type=refresh_token + client_id.
- REQUIRED device headers on all auth calls: X-Msh-Platform (kimi_cli /
  kimi_code_cli), X-Msh-Version, X-Msh-Device-Name, X-Msh-Device-Model,
  X-Msh-Os-Version, X-Msh-Device-Id (persisted uuid4 — the console
  device-list revocation handle; devices expire after 30 idle days).

## Wire

- PRIMARY: OpenAI chat completions at https://api.kimi.com/coding/v1
  (Authorization: Bearer <access_token>), streaming with
  stream_options.include_usage. Moonshot extensions:
  extra_body.thinking {type: enabled|disabled, effort?, keep?},
  max_completion_tokens (NOT max_tokens), prompt_cache_key,
  builtin_function tool type, x-trace-id response header.
- Anthropic Messages surface: https://api.kimi.com/coding/ +
  /v1/messages (?beta=true in first-party), token/key via x-api-key —
  needed only for catalog models flagged protocol:"anthropic"
  (adaptive thinking). Probe the exact URL join live before hardcoding.
- GET https://api.kimi.com/coding/v1/models (Bearer) → data[] with id,
  context_length, supports_reasoning/image_in/tool_use,
  supports_thinking_type, think_efforts, display_name, protocol.
- Extras on same base: /search, /fetch, /usages (per-window quota rows
  + booster wallet cents).
- Open-platform API keys: same protocols at api.moonshot.ai/v1 and
  /anthropic (zero-OAuth escape hatch; console issues up to 5
  subscription-scoped keys).

## Models/limits

Tiers: Andante → kimi-for-coding; Moderato+ → k3, k3-256k,
kimi-for-coding(-highspeed); k3[1m] = 1M context. K3 = 2026-07 flagship
(MoE, 1M ctx). ~300–1200 requests / 5h window, ≤30 concurrent streams.

## Token lifecycle (client-verified; server TTLs unpublished)

- Refresh when remaining < max(300s, expires_in/2); force on 401 then
  ONE retry; retryable refresh statuses 429/5xx (backoff), terminal
  401/403/invalid_grant → re-login.
- **REFRESH TOKENS ROTATE ON EVERY REFRESH** — a superseded token
  401s. First-party uses cross-process flock + atomic persist +
  re-read-on-401 + 300s rejected-token tombstone. Our daemon must
  serialize refresh through the vault (atomic write, re-read before
  retry) or concurrent processes log each other out. THIS IS THE
  BIGGEST RISK.

## Integration shape (cheapest correct)

Near-clone of our codex-flow reuse: new oauth profile (device flow
above) + vault-persisted device UUID + rotating-refresh serialization;
point the EXISTING OpenAI-compatible chat-completions adapter at
https://api.kimi.com/coding/v1 with Bearer; two adapter tolerances
(extra_body.thinking, max_completion_tokens); catalog ingestion from
the nonstandard /models (context_length is authoritative — first
non-codex source besides Gemini with real windows). Anthropic-protocol
models deferred until the catalog demands them. Secondary risk:
Moonshot never blessed client_id reuse (same posture as the codex flow
we already ship); API-key fallback exists.
