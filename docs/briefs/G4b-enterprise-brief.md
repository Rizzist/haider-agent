# G4b — enterprise providers: Azure OpenAI, Bedrock (mantle), Vertex (Claude)

Owner contract: "also support ... Enterprise Providers." Authority:
docs/research/g4-provider-breadth-seam-map.md +
docs/research/g-wave-external-api-research-2.md (§enterprise). Branch:
`g4b-enterprise`. G4a (local OSS) is SHIPPED — its keyless arm, origin
policy, and preset machinery are on main; build on them.

## Locked design decisions

1. AZURE OPENAI (v1 surface ONLY; classic ?api-version path out of scope):
   new add-card (AccountAddKind::AzureOpenAi) collecting resource endpoint
   (https://{res}.openai.azure.com or {res}.services.ai.azure.com) + API
   key. Profile: api_family OpenAiChatCompletions, base_url
   `{endpoint}/openai/v1`, provenance Custom, auth ApiKey with a NEW
   header mode — `api-key: <key>` instead of `Authorization: Bearer`
   (OpenAiHttp hardcodes Bearer; add an auth-header mode switch, the
   AnthropicAuthMode pattern). Model = DEPLOYMENT NAME in body.model.
   Discovery: probe GET {base}/models and TOLERATE 404/absence — on
   failure the card asks for deployment name(s) manually
   (configured_models + default_model). Strict origin policy (public
   HTTPS only).
2. BEDROCK — mantle surface ONLY (classic InvokeModel + SigV4 + AWS
   event-stream explicitly OUT OF SCOPE): new builtin provider id
   `bedrock`, api_family AnthropicMessages, base URL
   `https://bedrock-mantle.{region}.api.aws/anthropic` (region collected
   on the card, default us-east-1). Auth: bearer API key sent as
   `x-api-key` (the existing AnthropicAuthMode::ApiKey header — exact
   reuse), env import honors AWS_BEARER_TOKEN_BEDROCK via the env bridge.
   Standard `anthropic-version: 2023-06-01` header; model IN BODY with
   `anthropic.` prefixed names. No discovery API: seed configured_models
   with the documented set (anthropic.claude-fable-5, .claude-opus-5,
   .claude-opus-4-8, .claude-opus-4-7, .claude-sonnet-5,
   .claude-haiku-4-5), user-editable. Requires the AnthropicProvider to
   accept a parameterized endpoint (today OAuth pins its base; add an
   explicit `new_endpoint` constructor that only accepts the mantle URL
   shape — pin with a law).
3. VERTEX (Claude on Vertex): new builtin provider id `vertex`,
   api_family AnthropicMessages. Card collects project id + location
   (default `global`) + optional region endpoints. URL template
   `https://aiplatform.googleapis.com/v1/projects/{p}/locations/global/
   publishers/anthropic/models/{model}:streamRawPredict` (regional
   variant `https://{loc}-aiplatform.googleapis.com/...` when loc is not
   global). WIRE DELTAS: model lives in the URL, NOT the body; body
   carries `anthropic_version: "vertex-2023-10-16"` INSTEAD of model.
   Auth: OAuth2 Bearer with two credential sources — (a) pasted access
   token (mark short-lived ~1h in the card copy), (b) a refresh source
   that shells out to `gcloud auth print-access-token` (D-wave
   device-credential pattern; mock the shell in tests). Service-account
   JWT signing OUT OF SCOPE. Static model list seed (claude-opus-5,
   claude-sonnet-5, claude-fable-5, claude-sonnet-4-5@20250929,
   claude-haiku-4-5@20251001), user-editable.
4. EFFORT/FAST interaction (G3 is on main): the anthropic static effort
   ladder must recognize enterprise model naming — extend the base-model
   normalization to strip the `anthropic.` prefix and `@date` suffixes so
   `anthropic.claude-opus-5` and `claude-sonnet-4-5@20250929` hit their
   family rows (laws). FAST is Claude-API-only (research: not on
   Bedrock/Vertex): the fast gate refuses on `bedrock`/`vertex` provider
   ids regardless of model (law), and `supported_speeds` stays empty on
   their wire details.
5. ProviderCredentialSurface: add a `CloudBearer` surface (or reuse
   ApiKey deliberately — decide and DOCUMENT in the notes; the factory
   audit pin must stay honest either way).
6. Availability: bedrock/vertex profiles with a seeded model list are
   Available once a credential exists (no discovery requirement — extend
   the availability rule for seeded-list providers, pinned by a law).

## Mandatory laws

- LZ1 azure header mode: request golden with `api-key` header and NO
  Authorization header; deployment name rides body.model.
- LZ2 azure discovery-404 fallback → manual deployment entry persists and
  the profile lights Available.
- LB1 mantle golden: URL shape, x-api-key header, anthropic-version
  header, model in body, SSE decode of a scripted stream.
- LB2 endpoint pinning: `new_endpoint` refuses non-mantle URL shapes.
- LV1 vertex golden: URL template with model + :streamRawPredict, body
  has anthropic_version and NO model field, Bearer header.
- LV2 gcloud refresh source: mocked shell-out refreshes the vault
  credential; failure surfaces honestly.
- LE-x effort naming: `anthropic.claude-opus-5` and
  `claude-sonnet-4-5@20250929` resolve their family ladders; fast
  refused on bedrock/vertex (both directions).
- LA-x availability: seeded-list rule pinned; env bridge imports
  AWS_BEARER_TOKEN_BEDROCK.
- Goldens: rpc transcript only if new request types are added (prefer
  riding ProviderConfigure + existing login flows; if the transcript
  grows, regenerate + re-anchor honestly).

## Discipline

Standard lane rules (CARGO_INCREMENTAL=0; per-crate tests; fmt at every
commit; named-path adds; ledger truthful; notes + mutation-notes with ≥6
executed kills incl. the azure header mode, vertex body deltas, fast
refusal, and effort naming). No version bumps/tags/MCP/renames; no real
network calls (mock everything); never delete ~/.codex/sessions.
