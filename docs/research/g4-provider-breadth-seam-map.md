# G4 seam map — provider breadth (Explore agent, 2026-08-05, @ v0.0.71)

## Registry

- Provider ids are STRINGS: BUILTIN_PROVIDER_NAMES [7] provider lib.rs:75-83
  (anthropic, anthropic-oauth, openai, openai-oauth, openai-compatible,
  kimi-oauth, gemini). AuthMethod { ApiKey, OAuth } protocol
  credential.rs:26-31; CredentialDescriptor already carries optional
  base_url (:8-22).
- Durable registry (U-wave seam): daemon provider_registry.rs:54-65
  ProviderProfileV1 { provider_id, display_name, api_family, base_url,
  enabled, auth_requirement, configured_models, default_model, provenance }
  → providers.json (:24, store :73). Provenance {BuiltIn, Custom, Unknown}
  :45-51. Builtin seeds builtin_or_unknown :602-673.
- Wire: ProviderApiFamilyWire { AnthropicMessages, OpenAiResponses,
  OpenAiChatCompletions, GeminiGenerateContent, Unknown } rpc
  frame.rs:556-566; ProviderAuthRequirementWire { ApiKey, OAuth, None,
  Unknown } :576-582; ProviderSummaryWire :605-622.
- TUI /providers: render.rs:1537; keys app.rs:6405-6428 (e edit, h/z/g
  presets, digits device import). AccountAddKind app.rs:2345-2356.

## Base URLs + custom pattern

- Builtin consts: anthropic.rs:26-27, openai.rs:40-44, gemini.rs:24-25.
  OAuth adapters PIN base (openai.rs:338-342, :516-520; anthropic.rs:109).
  Sanctioned OAuth inference: daemon oauth.rs:242-258
  OAuthInferenceRegistration { base_url, auth_mode Bearer-only,
  header_set }; lookup sanctioned_inference :388.
- U-wave presets: app.rs:6759-6778 (HF router, OpenCode Zen/Go) →
  open_custom_preset :6783-6819 → submit_custom_add :6830-6868 →
  ProviderConfigure (CAS on registry revision) → ProviderRegistry::configure
  with ProductionProviderEndpointValidator (provider_registry.rs:32-43) →
  validate_openai_compatible_endpoint (openai.rs:2331). Blank custom
  default http://127.0.0.1:8000/v1 (app.rs:6714).
- URL derivation: compatible_endpoints openai.rs:2270-2323 (appends /v1,
  builds chat/completions + models; FORBIDS query strings — Azure classic
  conflict). SSRF fence: validate_compatible_origin :2372-2393 +
  blocked_credential_target :2551-2566 — HTTP only loopback;
  PRIVATE/LINK-LOCAL BLOCKED ENTIRELY (LAN Ollama blocker).

## Wire families → adapter binding

- anthropic.rs (+wire/mod.rs SSE machine): auth anthropic.rs:308-311
  ApiKey→x-api-key | OAuthBearer→Bearer + oauth beta (:28-29).
- openai.rs OpenAiProvider (Responses, responses_request_json :377,
  Bearer :213-228/:255, codex-lite header :256-260);
  OpenAiCompatibleProvider :420-642, CompatibleDialect { Generic,
  KimiOAuth }, chat_request_json :581.
- gemini.rs: {base}/{model}:streamGenerateContent (:301-307),
  x-goog-api-key (:182, 208).
- Factory: build_account_provider accounts.rs:4993-5138 matches
  (provider, auth_method). KEY SEAM :5033-5053 — any profile with
  api_family==OpenAiChatCompletions + ApiKey → OpenAiCompatibleProvider at
  profile.base_url (zero adapter code). NO keyless arm (:5127-5135 falls
  through to error).

## Accounts/secrets

- Vault trait accounts vault.rs:98-123; SecretHandle :24 (zeroizing);
  FileVault file_vault.rs:41; daemon default accounts.rs:350-356.
  Descriptors accounts.json (store.rs:24,37,169; one-active-per-provider).
  physical_alias(profile, provider, command) accounts.rs:538. Rotation
  resolver resolver.rs:68+. Env bridge env_bridge.rs:19.
- Custom key add flow: custom card → key card (app.rs:6886) →
  custom_login_target (accounts.rs:260-280) →
  validate_openai_compatible_key (:285-320, real probe) → vault + store +
  snapshot publish.
- Deepgram precedent: fixed vault alias transcription.deepgram
  (session_hub/rpc.rs:35-40), validate_key stt deepgram.rs:124.
- Device discovery (D-wave): device_discovery.rs:37+ per-provider probes
  (codex :114, claude :170, kimi :227, gemini :264 — ADC-import template).

## Model discovery + catalog

- CatalogSource { …, OpenAiCompatible { origin } } catalog.rs:114-126;
  endpoints :148-165 (compatible → GET {origin}/models); backstop
  :201-236 (HTTPS except loopback); discover_models :242-378 (SSRF-pinned,
  ETag, bounded); parser :382-558 (compatible = data[].id :426-441);
  catalog auth :560-580.
- Daemon provider→source: catalog_source accounts.rs:1666-1708 — customs
  with ChatCompletions + ApiKey/None → OpenAiCompatible origin. AuthReq
  None ACCEPTED for discovery (:1694-1696) but factory has no keyless arm.
- Persist: finish_provider_models_refresh accounts.rs:1525-1620 → sqlite +
  replace_models + republish. Availability REQUIRES non-empty discovered
  model list (provider_registry.rs:566-568).
- Unknown models degrade: context_window None ("never a guess"), pricing
  None, compatible_capabilities context_limit 0 (openai.rs:2219-2242).

## Addition paths

(a) LOCAL OSS — nearly zero adapter work: presets ollama
http://127.0.0.1:11434/v1 + lmstudio http://127.0.0.1:1234/v1 (+
AccountAddKind variants + keybindings). Gaps: (1) keyless arm in
build_account_provider + TUI key-card skip (AuthRequirement::None); or
placeholder-key convention ("ollama"); (2) LAN policy —
blocked_credential_target rejects private IPs; loosen for
Custom-provenance providers (allow RFC1918; keep metadata/link-local
blocked).

(b) ENTERPRISE:
- Azure: ChatCompletions with api-key HEADER (not Bearer) — OpenAiHttp
  header-mode switch needed (openai.rs:213-228, 255); v1 surface
  https://{res}.openai.azure.com/openai/v1/ has NO api-version query
  (avoids compatible_endpoints query ban); deployment name in body.model.
  New CompatibleDialect::Azure (Kimi pattern :430-434).
- Vertex: Messages body minus model + anthropic_version
  "vertex-2023-10-16" body field, URL
  …/publishers/anthropic/models/{m}:streamRawPredict, GCP OAuth Bearer.
  Needs base-URL-parameterized AnthropicProvider variant + token source
  (gcloud print-access-token refresh / ADC import via D-wave pattern).
- Bedrock: NO SigV4 anywhere in tree, NO aws crates. BUT bearer paths
  exist (see external research): bedrock-mantle
  https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages with
  x-api-key bearer + standard SSE + model in body — reuses Anthropic
  client nearly verbatim, newest models only. Classic InvokeModel needs
  AWS event-stream decoder + SigV4 — DEFER.
- Cross-cutting: ProviderCredentialSurface (lib.rs:153-158) has only
  Opaque|ApiKey|OAuthSubscriptionBearer — add CloudCredential surface so
  factory audit pin stays honest; AuthMethod/AuthRequirementWire may need
  a cloud variant (or reuse ApiKey deliberately + document).
