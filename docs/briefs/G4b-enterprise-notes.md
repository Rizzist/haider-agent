# G4b — enterprise providers: implementation notes

Brief: `docs/briefs/G4b-enterprise-brief.md`. Seam map:
`docs/research/g4-provider-breadth-seam-map.md` (anchors predate G4a/G3 —
code was trusted); API research:
`docs/research/g-wave-external-api-research-2.md` §enterprise. Branch
`g4b-enterprise`, implemented at v0.0.75 (no version bump). All six locked
decisions landed; deviations at the bottom. Azure classic api-version,
Bedrock classic InvokeModel/SigV4/event-stream, and service-account JWT
signing stayed OUT OF SCOPE.

## What shipped

### Azure OpenAI, v1 surface only (decision 1, LZ1/LZ2)

- `OpenAiHttp` gained an auth-header mode switch (the `AnthropicAuthMode`
  pattern): `OpenAiAuthHeaderMode { Bearer, AzureApiKey }`. One
  `with_auth_header` seam applies exactly ONE auth header — Bearer requests
  never carry `api-key`, Azure requests never carry `Authorization` — on
  both the chat POST and the models GET.
- `OpenAiCompatibleProvider::new_azure(credential, model, base)` constructs
  the azure mode under the STRICT origin fence (public HTTPS only, per the
  brief) and REFUSES non-azure origins, so the header mode can never leak a
  key to an arbitrary endpoint.
- ONE origin predicate carries every azure decision:
  `azure_openai_origin(origin)` — https on `{res}.openai.azure.com` /
  `{res}.services.ai.azure.com` with a validated resource label. It gates
  the adapter route in the factory (`compatible_adapter_route` → Azure),
  the login-validation adapter (`validate_openai_compatible_key`), the
  catalog discovery auth header (`CatalogAuthMode::AzureApiKey` — the v1
  surface documents `api-key` for keys; Bearer is Entra-only), and the
  seeded-inventory availability fallback.
- Azure profiles are ordinary CUSTOM chat-completions profiles: the
  `AccountAddKind::AzureOpenAi` card collects the resource endpoint and
  DEPLOYMENT name, derives `{endpoint}/openai/v1` (the v1 surface has no
  api-version query, so the G4a query-string ban never fires), rides the
  existing `provider.configure` + key-card chain, and body.model carries
  the deployment name.
- Discovery tolerance (LZ2): `GET {base}/openai/v1/models` may 404 —
  discovery failure is non-fatal because an azure-origin custom profile
  with configured deployments keeps them as its INVENTORY (see
  availability below). The card collected the deployment at create time,
  so the "manual deployment entry" is already durable.

### Bedrock — mantle surface only (decision 2, LB1/LB2)

- New builtin provider id `bedrock` (BUILTIN_PROVIDER_NAMES grew to 9).
  The registry seeds its profile: api_family AnthropicMessages, base_url
  `https://bedrock-mantle.us-east-1.api.aws/anthropic` (default region),
  auth ApiKey, enabled, `configured_models` = the documented six
  (`BEDROCK_SEED_MODELS`: anthropic.claude-fable-5/-opus-5/-opus-4-8/
  -opus-4-7/-sonnet-5/-haiku-4-5), default anthropic.claude-fable-5 —
  user-editable via configure.
- `AnthropicProvider::new_endpoint` accepts EXACTLY the mantle URL shape
  (`validate_bedrock_mantle_base_url` — DNS-safe region label, nothing
  else; LB2) and serves `{base}/v1/messages` with the bearer on
  `x-api-key` — the EXACT AnthropicAuthMode::ApiKey header path — plus the
  standard `anthropic-version: 2023-06-01` header, model IN THE BODY, and
  the unmodified SSE decoder (LB1).
- The TUI card collects the REGION (default us-east-1) and re-configures
  the builtin profile's endpoint; the registry allows the origin change
  ONLY through the mantle shape validator (see the identity-law deviation
  below). Commit chains the bearer-key card; validation runs at the
  profile endpoint with the profile's default model spelling.
- Env bridge (LA-x): daemon startup imports `AWS_BEARER_TOKEN_BEDROCK`
  through `haider_accounts::import_env` (alias `bedrock-env`, active
  ApiKey descriptor) exactly when the variable is set AND no bedrock
  descriptor exists — an explicit login/removal is never fought.

### Vertex (decision 3, LV1/LV2)

- New builtin provider id `vertex`, seeded with the documented model list
  (`VERTEX_SEED_MODELS`: claude-fable-5, claude-opus-5, claude-sonnet-5,
  claude-sonnet-4-5@20250929, claude-haiku-4-5@20251001) and NO endpoint —
  the card must supply project + location before the profile can serve.
- `AnthropicProvider::new_vertex` pins the publishers-models URL shape
  (`validate_vertex_models_base_url`: global template on the bare host, or
  `{loc}-aiplatform` host whose location AGREES with the path location)
  and applies the wire deltas (LV1): request URL
  `{base}/{model}:streamRawPredict` (model IN THE URL), body DROPS `model`
  and carries `anthropic_version: "vertex-2023-10-16"`, auth is a plain
  `Authorization: Bearer`, and neither the OAuth beta nor the standard
  `anthropic-version` HEADER is sent (Vertex versions through the body).
- Credential sources: (a) a pasted access token through the normal key
  card (~1h lifetime named in the card copy); (b) the gcloud refresh
  source (LV2, the D-wave device-credential pattern): discovery lists a
  "Google Cloud (gcloud ADC)" candidate when
  `application_default_credentials.json` exists (the file itself is NEVER
  read — its refresh token belongs to gcloud); import runs the mockable
  `gcloud auth print-access-token` shell-out (`GcloudAccessTokenSource`,
  production `GcloudCli` — no shell, bounded output, secret-free errors)
  and vaults the RESULT under the fixed `vertex-gcloud` alias; the broker
  re-runs the same command on auth failure and PERSISTS the fresh token
  before the in-turn retry (the attempt resolver treats the gcloud
  descriptor like OAuth: one refresh, then rotation semantics).

### Effort/fast interaction (decision 4, LE-x)

- `base_model` normalization (G3's seam) now strips the Bedrock
  `anthropic.` prefix and the Vertex `@YYYYMMDD` suffix before the family
  match, so `anthropic.claude-opus-5` and dated slugs hit their family
  rows in the static effort tables, the clamp, the fast gate, and
  `model_capabilities` (context windows). A malformed suffix (`@2026`)
  does NOT normalize. Unknown families still get the EMPTY row.
- FAST is Claude-API-only, refused on `bedrock`/`vertex` at every gate:
  `validate_fast` (toggle time — the provider match stays first-party
  anthropic only), `anthropic_fast_for` (construction time — now takes the
  provider id, so the normalized model gate cannot re-admit the enterprise
  spellings), and `model_detail_wire` keeps `supported_speeds` EMPTY on
  bedrock/vertex details while first-party details keep advertising fast.
- `/effort` works on the enterprise pairs: `effort_ladder`'s static
  fallback arm and the wire-detail enrichment include bedrock/vertex.

### ProviderCredentialSurface (decision 5)

`CloudBearer` was ADDED for the vertex adapter (a GCP platform bearer that
is neither a vaulted vendor API key nor a release-owned OAuth
subscription). Bedrock mantle deliberately stays `ApiKey`: its bearer
rides the byte-identical `x-api-key` header path of the first-party key
mode, so a distinct surface would claim a difference the wire does not
have. Both pinned in `g4b_factory_builds_bedrock_and_vertex_adapters_with_their_surfaces`
and the LB1/LV1 goldens.

### Availability (decision 6, LA-x)

`summaries()`/`summary()` now take an explicit `has_credential` predicate
— every caller states its account truth (the accounts actor passes
`provider_has_credential(accounts)`; init-time id listing passes a false
predicate). With nothing discovered, a SEEDED-inventory profile
(bedrock/vertex always; customs only at azure origins) serves its
configured models as the inventory, enriched through the same
`model_detail_wire` path; it lights Available exactly when `enabled` AND a
credential exists AND an endpoint is configured, with honest reasons
("provider has no credential" / "provider endpoint is not configured")
otherwise. Non-azure customs keep the G4a discovery-only rule unchanged.
Because a login/import can now FLIP availability, `finalize_and_respond`
and the gcloud import publish the FULL provider view (login previously
published accounts only). Default-model selection validates against the
seeded inventory too (`selectable_slugs`).

## Laws (all by name)

- LZ1: `lz1_azure_request_rides_api_key_header_and_deployment_model`,
  `azure_origin_predicate_and_constructor_agree_both_directions`
  (provider openai tests);
  `azure_origin_custom_profiles_route_through_the_api_key_header_adapter`
  (daemon accounts tests — the factory route mapping).
- LZ2: `lz2_azure_custom_keeps_manual_deployments_available_without_discovery`
  (daemon registry tests).
- LB1: `lb1_bedrock_mantle_golden_url_headers_body_and_sse`.
- LB2: `lb2_new_endpoint_refuses_non_mantle_url_shapes`.
- LV1: `lv1_vertex_golden_model_in_url_version_in_body_bearer_header`,
  `vertex_base_url_shape_is_pinned_global_or_matching_regional`.
- LV2: `lv2_gcloud_refresh_source_refreshes_vault_and_surfaces_failure`,
  `lv2_gcloud_device_import_vaults_the_token_and_lights_vertex`.
- LE-x: `le_enterprise_model_names_resolve_their_family_rows` (provider
  effort tests),
  `le_bedrock_and_vertex_pairs_validate_effort_but_refuse_fast` (daemon
  model_select tests),
  `provider_tuning_derives_from_metadata_and_fast_gate_filters_stale_pairs`
  (extended with the provider dimension),
  `bedrock_and_vertex_model_details_get_effort_ladders_but_no_speeds`.
- LA-x: `la_seeded_list_providers_light_available_once_a_credential_exists`,
  `la_env_bridge_imports_aws_bearer_token_bedrock`.
- Identity/origin: `enterprise_origin_reconfigure_is_shape_validated`
  (create-only law half re-pinned by the pre-existing
  `existing_custom_provider_identity_fields_are_create_only`).
- Factory/login: `g4b_factory_builds_bedrock_and_vertex_adapters_with_their_surfaces`,
  `enterprise_login_validates_at_the_profile_endpoint_with_its_default_model`.
- TUI (`tests/g4b_enterprise_cards_tests.rs`):
  `azure_card_derives_the_v1_base_and_chains_the_key_card`,
  `bedrock_card_builds_the_mantle_url_and_echoes_the_seeded_inventory`,
  `vertex_card_collects_project_and_location`,
  `enterprise_footer_buttons_and_edit_routing`.

## Goldens / wire

No new rpc request types, no protocol variant renames: the enterprise
cards ride `provider.configure` (its wire already carried
`api_family`/`models`/`default_model`) and the existing
`account.login_api` / `account.import_device` flows, so the rpc transcript
is unchanged. New request-shape goldens live at the provider layer (LZ1,
LB1, LV1). `ProviderCredentialSurface` gained `CloudBearer` (a Rust enum,
not a wire type). The `CredentialValidator` trait gained an `endpoint`
parameter (in-crate trait, tests updated).

## Ledger

`cargo run -p xtask -- test-count --update`: 1988 → 2010 (+22: 7
haider-provider [4 anthropic, 1 effort, 2 openai], 11 haider-daemon [4
registry, 1 model_select, 6 accounts], 4 haider-tui).

## Deviations

- **Enterprise origins are mutable, shape-pinned** — the create-only
  identity law (W10b) refuses origin changes on existing profiles, but the
  bedrock/vertex builtins are SEEDED at init and their endpoints carry
  user coordinates (region / project+location). Rather than smuggling the
  endpoint onto credential descriptors, the law gained a scoped exception:
  ONLY these two ids may change origin, ONLY through their URL-shape
  validators (`enterprise_origin_validator`), pinned by
  `enterprise_origin_reconfigure_is_shape_validated`. api_family and
  auth_requirement stay create-locked everywhere.
- **The gcloud import is deliberately receipt-free** — every other durable
  account mutation claims a receipt; the gcloud arm does not, because
  re-import IS refresh (each run mints a fresh short-lived token), a
  replayed command is idempotent by construction, and no receipt may carry
  a secret — the vault file is the durable truth (the
  `transcription.secret_set` precedent). Documented in the handler.
- **`import_env` takes `&dyn Vault`** — was `&impl Vault`; the daemon
  hands it an `Arc<dyn Vault>`. Call sites coerce unchanged.
- **Azure validation identity** — an azure custom's login validation runs
  through `new_azure` (api-key header) instead of `new_custom`; without
  this the 1-token probe would 401 a working key. Route chosen by the one
  origin predicate.
- **`summaries()` signature change** — decision 6 needs account truth in
  the summary; instead of a second summaries API that could silently serve
  stale availability, the signature forces every caller to pass a
  predicate (compile-error-audited across the actor).
- **TUI band math** — the /providers//accounts footer grew a sixth button
  row and an `enterprise:` hint line; the three pre-existing band
  assertions (f2 scroll, w5e anchor, d2 walk height) were updated
  honestly, and Custom stays the last button (B6b edge rule).
- **`model_capabilities` normalizes too** — enterprise spellings report
  their real context windows (1M for the 5-family) instead of the unknown
  100k row; same one `base_model` authority, no new guessing.
