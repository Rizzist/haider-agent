# B6 research — provider breadth (Gemini first)

Fable seam research, 2026-08-01. Line numbers approximate. No Gemini/
Google adapter exists in Rust today; only the TUI sim anticipates one.

## Q1 — trait surface (small; API-key-only is fully within contract)

- Provider trait (haider-provider/src/lib.rs ~275-284): just
  credential_surface() (default Opaque; ApiKey variant exists),
  capabilities() -> CapabilityDoc, stream_turn(TurnRequest) ->
  ProviderStream. No associated types; OAuth NOT demanded.
- TurnRequest ~157-169: messages/model/max_tokens/system_prompt/tools
  (ToolDefinition name+description+input_schema)/attachments.
- Canonical IR: Block (protocol/provider.rs ~13-39) incl.
  ProviderOpaque{provider,data} — keyed by provider name, replayed
  only to the same family; foreign opaque rejected (wire/mod.rs
  ~153-161). Compactor explicitly ignores it (worker.rs ~215-219).
- StreamEvent ~42-76: TextDelta/ReasoningDelta/RefusalDelta/
  ProviderOpaque/ToolCallStart/ArgsDelta/End/UsageUpdate/Finish;
  FinishReason incl. Cancelled (an outcome, never an error).
- Errors ~171-227: Authentication/PermissionDenied/RateLimited/
  Overloaded/ContextExceeded/InvalidRequest/Transport/MalformedFrame/
  InvalidUtf8/Internal; default_retryable = RateLimited|Overloaded|
  Transport; retry_after_ms rides the error.
- Stream contract: SSE; terminal Finish OR one typed error OR
  silence-until-drop; Utf8Assembler guarantees complete-UTF-8 deltas;
  ProviderStream drop aborts producer. Timeouts 10/30/90s;
  RetryPolicy::Never in-adapter — R6 retry is actor-owned
  (MAX_PROVIDER_ATTEMPTS=3, blake3 full-jitter backoff,
  ProviderAttemptResolver lets the daemon rotate accounts mid-turn).

## Q2 — templates

- anthropic.rs 567 + wire/mod.rs 729 (encoder + typed-event SSE
  decoder); openai.rs 2538 (Responses + Chat, two decoders, SSRF
  guards); origin.rs 239 (FixedOriginGuard resolve-validate-pin).
- Shared: Utf8Assembler, origin guards, sanitized error prose,
  replay_* fixture helpers, ERROR_BODY_LIMIT 64KB bounded reads,
  no-leak law (journal sanitized; bounded prose → daemon.log only).
- Duplicated per adapter: SSE framer, transport_error/parse_retry_
  after, HTTP-status classifier, reqwest construction.
- BEST TEMPLATE for Gemini: anthropic.rs STRUCTURE (one path, one
  decoder, one auth header) + ChatDecoder LOGIC (openai.rs ~1144-1398)
  — Gemini streamGenerateContent?alt=sse emits data-only frames each a
  complete GenerateContentResponse (same shape as chat completions).

## Q3 — registry + selection

- BUILTIN_PROVIDER_NAMES = [anthropic, anthropic-oauth, openai,
  openai-oauth, openai-compatible] (lib.rs ~54-61); "fake" only via
  injected test factory. creatable_providers() (worker.rs ~430-440) is
  the ONE session.create authority.
- Per-turn: AccountsProviderFactory::resolve_for_turn →
  build_account_provider (accounts.rs ~4745-4857) — the
  match (provider, auth_method) THE SLOT for a Gemini arm.
- provider_registry.rs: durable providers.json; ProviderProfileV1
  (provider_id, api_family, base_url, auth_requirement, models…);
  builtin_or_unknown seeds families; unknown → "adapter not
  registered". RPCs provider.list/models_refresh/configure/remove.
- catalog.rs: CatalogSource::{OpenAiSubscription, AnthropicSubscription,
  OpenAiCompatible}; SSRF-pinned, 1MB bound, ETag; "DISCOVERY IS NEVER
  SYNTHESIZED" — no static fallback; durable provider_models cache.
  bearer_auth ONLY today (Gemini needs x-goog-api-key — real seam
  change); catalog_source serves only OAuth builtins + custom (plain
  API-key builtins have no refresh source today). Adapters carry
  hardcoded capability tables for context limits.

## Q4 — accounts + auth (API-key path end-to-end)

- /login <provider> api → vault.stage (raw secret, same-UID UDS,
  TTL 300s) → account.login_api → ProviderCredentialValidator
  (supports = anthropic|openai TODAY; accounts.rs ~176-248) fires a
  real 1-token ping through the actual adapter → FileVault
  (<root>/<hex(alias)>.vault, 0600, 512KB cap) → SecretHandle only
  (no Clone/Serialize, zeroizing) → CredentialDescriptor{provider,
  auth_method, base_url?, active} → R6 resolve_for_turn → adapter
  builds sensitive header per request.
- Gemini needs: name in BUILTIN_PROVIDER_NAMES, builtin_or_unknown
  arm, validator arm, build_account_provider arm; NO descriptor schema
  change; header x-goog-api-key. Env import exists
  (import_env, alias <provider>-env; HAIDER_ANTHROPIC_API_KEY
  precedent in live tests).

## Q5 — tool loop

- ToolManifest → ToolDefinition (provider_definition worker.rs ~3709).
- Encodings: Anthropic tools[]/tool_use/tool_result; Responses
  function items; Chat function/tool_calls. All decoders normalize to
  ToolCallStart → ArgsDelta* → End.
- Gemini: tools:[{functionDeclarations:[…]}]; functionCall args arrive
  COMPLETE per part → emit start + one full-args delta + end
  (FakeProvider emit_tool_call pattern). Replay results as
  functionResponse parts — NAME-KEYED (ids only in newer revisions):
  adapter must synthesize deterministic call_ids on decode and map
  back on encode.

## Q6 — test pattern (4 layers per adapter)

In-crate unit (payload/headers/SSRF pins incl. mutation-check
proxying); fixture replay vs golden StreamEvent sequences (7-byte
chunking exercises splits; manifest.json per provider under
tests/fixtures/<provider>/); live gated (#[ignore] +
HAIDER_LIVE_PROVIDER_TESTS + HAIDER_<P>_API_KEY) with a fixture-
PROMOTION harness capturing sanitized real payloads; catalog
shape tests. gemini needs all four + fixtures for text/functionCall/
usageMetadata/finishReason/429/400/safety-block/malformed.

## Q7 — TUI/UX

- push_account_add_buttons (render.rs ~889-941) shared by /accounts +
  /providers — Gemini = one AccountAddKind + label + click arm +
  /login gemini api parsing. Sim ALREADY anticipates google: mock seed
  gemini-key/google (mock.rs ~333-340); w5d_providers_tests pins
  "google  ○ adapter not installed"; w5e3 picker expects google —
  these tests FLIP when the adapter lands.
- No `haider login` CLI subcommand (TUI/RPC only); run --provider
  validated daemon-side.

## Gemini adapter shape (condensed)

gemini.rs cloned from anthropic.rs structure: POST
https://generativelanguage.googleapis.com/v1beta/models/{model}:
streamGenerateContent?alt=sse, header x-goog-api-key,
credential_surface ApiKey, RetryPolicy::Never, 10/30/90s. Body:
system_instruction + contents (user|model roles; parts text/
inlineData/functionCall/functionResponse) + functionDeclarations +
generationConfig{maxOutputTokens}. Decoder: SseFramer + ChatDecoder-
style machine (text→TextDelta, thought→ReasoningDelta, thought
signatures→ProviderOpaque{gemini}, functionCall→full-args triple,
usageMetadata→UsageUpdate, finishReason STOP/MAX_TOKENS/SAFETY→
Finish/Refusal). HTTP classify: 400 INVALID_ARGUMENT (+token-count
prose→ContextExceeded), 401/403, 429 RESOURCE_EXHAUSTED (RetryInfo→
retry_after), 503→Overloaded. Catalog: CatalogSource::GeminiApiKey →
GET /v1beta/models (FixedOriginGuard generativelanguage.googleapis.com,
x-goog-api-key auth mode — NEW seam: discover_models is bearer-only
and API-key builtins have no catalog_source today), models[].
inputTokenLimit → context_window (first non-codex source with real
windows) + static capability backstop.

## Effort + risk

~2-3k new lines, ~10 files, 4 crates (provider/daemon/rpc/tui).
Design decisions (not mechanical): new ProviderApiFamilyWire variant
(negotiation/golden/tolerance tests), discover_models auth mode +
vault access at refresh, call_id synthesis/replay, thought-signature
opaque round-trip. BIGGEST RISK: multi-turn tool-loop replay fidelity
(name-keyed functionResponse vs call_id-centric history) — fails on
the SECOND tool turn, the poison-every-session class (W5g-6 autopsy
precedent). Fixture tests MUST include a full two-turn tool
round-trip with a continuation-request payload golden.
