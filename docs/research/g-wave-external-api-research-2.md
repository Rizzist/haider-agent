# G-wave external API research, part 2 (verified 2026-08-05)

Part 1 (Anthropic effort/fast/thinking/TodoWrite) in
g-wave-external-api-research.md. This file: non-Anthropic effort params
(G3) + OSS/enterprise endpoints (G4). Primary-doc sourced; unconfirmed
items flagged inline.

## G3 — effort per provider

- OPENAI: Responses `body.reasoning: {effort, summary}`; ChatCompletions
  top-level `reasoning_effort`. Values (model-dependent): none, minimal,
  low, medium, high, xhigh, max. gpt-5.5 default medium. Codex CLI enum
  adds `ultra` but CONVERTS ultra→max on the wire; client default Medium;
  summary sent only if model_info.supports_reasoning_summary_parameter;
  include: ["reasoning.encrypted_content"] always; max_output_tokens NEVER
  present. Lite (WS/`x-openai-internal-codex-responses-lite: true`):
  reasoning.context "all_turns" only when lite; parallel_tool_calls false.
  Changing effort mid-session invalidates previous_response_id anchor →
  full-history resend (accepted, codex issue #32533). Our catalog already
  parses supported_reasoning_levels per model — validate against it.
- GEMINI: generationConfig.thinkingConfig.thinkingLevel (Gemini 3.x):
  "minimal"|"low"|"medium"|"high". 3.1-pro: low/med/high, default high,
  cannot disable. 3-flash: all four, default high. 3.6/3.5-flash: all
  four, default medium. 2.5 era: thinkingBudget ints (2.5 Pro 128-32768
  no-disable; 2.5 Flash 0-24576, 0 disables; -1 dynamic).
  thinkingLevel + thinkingBudget together → 400. includeThoughts bool for
  summaries.
- KIMI: top-level `thinking: {"type": "enabled"|"disabled"}` (+
  `keep: "all"` preserved-thinking on k2.6). k2.5 default-on toggleable;
  k2.7-code always-on not configurable; k3 always-thinking controlled by
  top-level `reasoning_effort: "low"|"high"|"max"` default max.
  reasoning in `reasoning_content` field (non-namespaced). Don't set
  temperature for k2.7-code/k2.6. Matches our catalog think_efforts +
  supports_thinking_type extensions.
- OPENROUTER: body.reasoning {effort|max_tokens (mutually exclusive),
  enabled, exclude}; effort max/xhigh/high/medium/low/minimal/none;
  provider-mapped downstream.
- XAI GROK: top-level reasoning_effort; grok-4.5 low/med/high default
  high, cannot disable; grok-4.20-multi-agent adds xhigh (=agent count);
  grok-4.3 none/low/med/high (SECONDARY SOURCE — unconfirmed primary).
- Gotcha: four different vocabularies — keep effort a per-pair validated
  string, never a global enum.

## G4 — OSS local

- OLLAMA: base http://localhost:11434/v1; auth none (compat layer wants a
  placeholder key, convention "ollama"); GET /v1/models (OpenAI shape) or
  native /api/tags (richer); context length via POST /api/show →
  model_info["{arch}.context_length"] + capabilities array
  (tools/vision) — use to gate tools per model. Tools supported on
  /v1/chat/completions incl. streamed; tool_choice IGNORED; base64 images
  only. /v1/responses exists (non-stateful) since v0.13.3. Standard SSE.
- LM STUDIO: base http://localhost:1234/v1; auth OPTIONAL (Developer-page
  tokens → Bearer). /v1/models, chat/completions, /v1/responses; tool use
  supported; also ANTHROPIC-COMPAT /v1/messages (existence confirmed,
  deltas unverified — smoke test). Native /api/v0/models has
  max_context_length + state (loaded); /api/v1/* stateful (0.4.0+). JIT
  load + idle TTL auto-evict.
- GENERIC (vLLM :8000, llama.cpp :8080): assume {base}/chat/completions +
  {base}/models + optional Bearer. Tolerances REQUIRED: missing [DONE];
  SSE comment/ping lines; usage only with stream_options include_usage
  (some servers reject stream_options — treat missing usage as normal);
  tool_choice ignored/flag-gated (vLLM --enable-auto-tool-choice,
  llama.cpp --jinja); tool_call ids absent/non-unique; finish_reason
  "stop" instead of "tool_calls"; extra fields (reasoning_content,
  timings) — unknown-field-tolerant deserialization mandatory; /v1/models
  may 404 or return synthetic entries. llama.cpp /v1/models includes
  meta.n_ctx_train.

## G4 — enterprise

- AZURE OPENAI: RECOMMENDED v1 surface (GA):
  https://{res}.openai.azure.com/openai/v1/ — NO api-version param; auth
  `api-key: <key>` header OR Bearer Entra token; Responses API supported;
  DEPLOYMENT NAME goes in body.model. No confirmed data-plane deployment
  listing (flag) → require deployment name as user config, skip
  discovery (or probe /openai/v1/models and tolerate 404). Classic
  deployment-scoped path (…/openai/deployments/{dep}/chat/completions
  ?api-version=2024-10-21, api-key header) still exists — has query
  string, no Responses; support later if at all.
- AWS BEDROCK: bearer keys are GA — env AWS_BEARER_TOKEN_BEDROCK;
  `Authorization: Bearer` on bedrock-runtime.{region}.amazonaws.com; IAM
  action bedrock:CallWithBearerToken. NO SigV4 REQUIRED. Two surfaces:
  (1) classic InvokeModelWithResponseStream — Messages body minus model
  (model in URL, URL-encode ':'), anthropic_version "bedrock-2023-05-31",
  inference-profile prefixes REQUIRED (global./us./eu.… else 400),
  streaming is BINARY AWS EVENT-STREAM (not SSE) → needs frame decoder —
  DEFER; (2) NEW "Claude in Amazon Bedrock" / mantle:
  https://bedrock-mantle.{region}.api.aws/anthropic/v1/messages — REAL
  Messages API, model in body (anthropic.claude-fable-5 style names),
  anthropic-version 2023-06-01 header, STANDARD SSE, bearer via x-api-key
  (set Anthropic client base_url + api key = bearer token). Newest models
  only (Fable 5, Opus 5/4.8/4.7, Sonnet 5, Haiku 4.5). SHIP MANTLE FIRST.
- GCP VERTEX (Claude): global endpoint (recommended, required newest):
  https://aiplatform.googleapis.com/v1/projects/{p}/locations/global/
  publishers/anthropic/models/{m}:streamRawPredict; regional variant only
  ≤ sonnet-4.6. Body = Messages minus model, plus body field
  anthropic_version "vertex-2023-10-16". Model ids: newer plain
  (claude-opus-5), older @date (claude-sonnet-4-5@20250929). Auth OAuth2
  Bearer from ADC ONLY (no API key) — token source = gcloud auth
  print-access-token / service-account JSON / gcp_auth crate. SSE on
  streamRawPredict (standard behavior; content-type unconfirmed — smoke
  test). No models API — static list.

## Architecture takeaways

1. One generic OpenAI-compat client (existing OpenAiCompatibleProvider)
   covers Ollama/LM Studio/vLLM/llama.cpp/Azure-v1, parameterized by base
   URL + header mode (Bearer vs api-key) + tolerance list.
2. Existing Anthropic Messages client covers Vertex (URL/model/version
   deltas + token source) and Bedrock mantle (URL + x-api-key).
3. Only genuinely new plumbing = Bedrock classic (SigV4 + event-stream) —
   deferred; bearer paths ship first.
