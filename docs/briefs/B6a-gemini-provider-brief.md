# B6a — Gemini provider: adapter, catalog, accounts, registry

AUTHORITY: docs/research/b6-provider-breadth-research.md (read WHOLE,
first). Line numbers approximate — re-locate every seam. TUI button/
login-card wiring is a SEPARATE UI lane (B6b) — NO haider-tui changes
here, but note the sim tests that pin "google ○ adapter not
installed" (w5d_providers_tests, w5e3_picker) — if daemon-side
changes flip them, coordinate by leaving them RED-listed in your
summary rather than editing haider-tui.

## Scope

1. **Adapter** `crates/haider-provider/src/gemini.rs` (+ in-crate
   gemini_tests.rs): anthropic.rs structure, ChatDecoder-style
   data-only SSE decode per the research's "Gemini adapter shape" —
   endpoint, x-goog-api-key sensitive header, credential_surface
   ApiKey, RetryPolicy::Never, 10/30/90s timeouts, Utf8Assembler,
   FixedOriginGuard, ERROR_BODY_LIMIT bounded classifier (400
   INVALID_ARGUMENT + token-count prose → ContextExceeded; 429
   RESOURCE_EXHAUSTED + RetryInfo → RateLimited with retry_after;
   503/overload prose → Overloaded; no-leak law: journal sanitized,
   bounded prose to stderr only). functionCall → synthesized
   deterministic call_ids (start + ONE full-args delta + end);
   functionResponse replay maps call_ids back to names; thought
   signatures round-trip as ProviderOpaque{provider:"gemini"} and
   foreign opaque is rejected. Safety block → RefusalDelta + Finish.
   CapabilityDoc: vision Native, parallel_tools per API reality,
   static context-limit backstop table.
2. **Catalog**: CatalogSource::GeminiApiKey → GET /v1beta/models
   (pinned origin, 1MB bound, ETag), parse models[] name (strip
   models/ prefix) + inputTokenLimit → context_window. Extend
   discover_models auth from bearer-only to per-source header mode —
   ADDITIVE, existing sources byte-identical. Wire catalog_source for
   the gemini API-key builtin (vault access at refresh per the
   existing custom-provider pattern).
3. **Registry/accounts wiring**: "gemini" in BUILTIN_PROVIDER_NAMES;
   builtin_or_unknown profile (new ProviderApiFamilyWire variant, e.g.
   GeminiGenerateContent — additive; negotiation + wire goldens +
   older-client tolerance re-proved); ProviderCredentialValidator
   supports + validate arm (real 1-token ping through the adapter);
   build_account_provider arm (GEMINI, ApiKey); env import alias
   works out of the box. NO descriptor schema change.
4. **Tests**: all four layers per the research — unit (payload shape,
   header, SSRF pin w/ mutation-check proxy), fixture replay
   (tests/fixtures/gemini/ + manifest: text, functionCall,
   usageMetadata, finishReason variants, 429/400/safety/malformed;
   7-byte chunk splits; golden StreamEvent sequences), live-gated
   gemini_live_tests.rs (HAIDER_LIVE_PROVIDER_TESTS +
   HAIDER_GEMINI_API_KEY + promotion harness), catalog shape tests.
   MANDATORY: a full TWO-TURN tool round-trip fixture (call → result →
   continuation-request payload golden) — the name-keyed
   functionResponse vs call_id-centric history mismatch fails on the
   SECOND tool turn (poison-every-session class, W5g-6 precedent).

## Laws (minimum)

- gemini_stream_decodes_text_reasoning_toolcall_usage_finish (golden).
- two_turn_tool_roundtrip_continuation_payload_is_stable (golden).
- synthesized_call_ids_are_deterministic_and_replay_maps_to_names.
- foreign_provider_opaque_is_rejected / gemini_opaque_roundtrips.
- http_errors_classify_typed_without_leaking_bodies (429 retry_after,
  400 context prose, 503, safety block).
- catalog_parses_models_with_context_windows; existing catalog
  sources byte-identical (golden).
- api_family_wire_addition_tolerated_by_older_clients (golden).
- validator_ping_uses_real_adapter_and_stores_no_secret_in_errors.
- session_create_accepts_gemini_when_account_active.

Standing lane laws: tests never inline; mutation-notes doc with
RUNTIME failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D
warnings; additive protocol only; ledger update; NO haider-tui; no
Cargo.lock; no version bumps; leave changes uncommitted; run no git
commands. Use up to 3 research subagents and 2 verify subagents.
Finish with a summary of files changed and tests added.
