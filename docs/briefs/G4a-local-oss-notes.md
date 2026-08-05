# G4a — local OSS providers: implementation notes

Brief: `docs/briefs/G4a-local-oss-brief.md`. Seam map:
`docs/research/g4-provider-breadth-seam-map.md`; API research:
`docs/research/g-wave-external-api-research-2.md` (§G4 OSS local,
§generic). Branch `g4a-local-oss`, implemented at v0.0.73 (no version
bump). All six locked decisions landed; deviations at the bottom.
Enterprise (Azure/Bedrock/Vertex) untouched — G4b.

## What shipped

### Origin policy (decision 3, LK3)

- `CompatibleOriginPolicy { Strict (default), TrustedLan }` (provider
  openai.rs, exported from lib.rs). `TrustedLan` exempts EXACTLY RFC1918
  (10/8, 172.16/12, 192.168/16, incl. IPv4-mapped IPv6) from the blocked
  set and allows plain HTTP to loopback OR RFC1918. Everything else the
  strict fence refuses stays refused: link-local `169.254.0.0/16` (cloud
  metadata), multicast, unspecified/broadcast, 0/8, IPv6 ULA + link-local,
  and PUBLIC plain HTTP.
- Threaded through `compatible_endpoints`, `validate_compatible_origin`,
  `CompatibleOriginGuard` (resolve-validate-pin keeps working on
  hostnames that resolve to LAN addresses), and
  `validate_openai_compatible_endpoint(origin, policy)`.
- Who gets which policy: `OpenAiCompatibleProvider::new` (builtin) and the
  Kimi constructor stay Strict byte-for-byte; new `new_custom` is
  TrustedLan. `ProductionProviderEndpointValidator` (daemon
  provider_registry.rs) validates with TrustedLan — it only ever runs for
  brand-new `provider.configure` profiles, which are Custom provenance by
  construction. `validate_openai_compatible_key` routes custom login
  targets through `new_custom` so a stored key validates against the LAN
  origin it will serve from. The catalog backstop
  (`openai_compatible_catalog_endpoint`) applies the same matrix — the
  `OpenAiCompatible` source exists only for Custom-provenance profiles
  (accounts.rs `catalog_source` requires it).
- The factory picks policy by provider id: `custom_compatible_adapter`
  gives custom ids TrustedLan and any builtin id (defensive; none routes
  there today) Strict.

### Decoder tolerances (decision 4, LK4–LK9)

Chat decoder (openai.rs), one law per tolerance, scripted SSE fixtures in
`tests/openai_provider_tests.rs`:

- LK4 missing `[DONE]`: EOF after a delivered `finish_reason` completes
  cleanly (this half was already tolerant — now pinned); EOF before any
  finish_reason stays a malformed stream.
- LK5 `:` comment/ping lines skipped (pre-existing framer behavior,
  pinned; see mutation notes for the redundancy finding).
- LK6 absent stream usage: no UsageUpdate, no error. `stream_options:
  {"include_usage": true}` is still sent (existing request golden pins it).
- LK7 absent tool-call ids: the decoder mints the stable per-index id
  `tool-call-{index}`; later deltas on the same index correlate to it, and
  a later-arriving id on a synthesized index is informational (never a
  consistency violation). A missing NAME still fails — a call without a
  name is unrunnable.
- LK8 finish_reason `"stop"` with open tool calls: the calls complete
  (ToolCallEnd) and the turn finishes `ToolUse`, on both the `[DONE]` and
  EOF paths. MaxTokens/Refusal closes still drop partials (pre-existing
  laws pin that).
- LK9 unknown extra fields (`timings`, `system_fingerprint`, vendor
  objects at chunk/choice/delta level) are invisible to decoding.

### Keyless auth arm (decision 2, LK1)

- Resolution (`AccountsProviderFactory::resolve_provider`): a
  `CredentialMissing` failure for an ENABLED custom chat-completions
  profile with auth requirement None (empty `auth_methods` on the wire
  summary) falls back to `keyless_account` — a synthesized
  `{provider}-keyless` descriptor at the profile origin plus the
  placeholder credential. Runs on both the broker and non-broker paths
  (both surface CredentialMissing). A stored key always wins: the
  fallback only runs when resolution found no credential at all.
- Placeholder: `KEYLESS_PLACEHOLDER_BEARER = b"ollama"` (ollama's compat
  layer wants a non-empty key; LM Studio ignores the header), minted
  through MemoryVault so the SecretHandle redaction/zeroization laws
  hold. Wire golden: `Authorization: Bearer ollama` on both GET /models
  and POST /chat/completions.
- Factory: `build_account_provider` gains the keyless arm (family
  ChatCompletions + empty auth_methods). The key-requiring profile arm
  now EXCLUDES empty-auth profiles so the new arm is load-bearing —
  deleting it is an observable "no account-backed adapter", not a silent
  fallthrough.

### Discovery + availability (decision 5, LK2)

- No new discovery machinery: `catalog_source` already accepted auth-None
  customs; LK2 pins configure(auth None) → persisted Custom/None profile
  → credential-free `discover_models` against a mock loopback
  `/v1/models` → `replace_models` → summary Available. `context_window`
  stays None (never a guess); `/api/show` is NOT probed.
- Empty/unreachable discovery leaves the profile Unavailable (existing
  rule); the TUI row hint for keyless customs says
  "unavailable — start the server, then refresh (f)".

### TUI presets (decision 1, LK10)

- `/providers`: `o` → Ollama (`http://127.0.0.1:11434/v1`), `l` →
  LM Studio (`http://127.0.0.1:1234/v1`) — h/z/g taken, o/l were free.
  Both open the SAME custom card, keyless. New `f` re-runs
  `provider.models_refresh` for the selected provider (the affordance
  behind the row hint). `/accounts` add rows gain `+ Ollama (local)` and
  `+ LM Studio (local)` on their own row; Custom stays last.
- `CustomProviderCard.keyless` rides AppRequest::ProviderConfigure →
  LiveCommand::ConfigureProvider → wire `auth_requirement: none`
  (ApiKey otherwise — unchanged for h/z/g/custom). Edit cards inherit
  keyless from the summary (empty auth_methods) so re-configuring an
  auth-None profile does not forge an `api_key` identity mismatch.
- Commit flow: a keyless commit SKIPS the masked key card and chains
  straight into `AppRequest::ProviderModelsRefresh` with the "keyless —
  discovering models…" message. Keyed customs keep the key-card chain
  byte-for-byte.
- Keyless customs have no account row, so `provider_model_refreshes`
  grew a summary-driven trigger: enabled chat-completions custom with a
  stored origin, no auth methods, and no models asks once per connection
  (same `models_requested` dedup).
- The providers key map split into an action line + preset line
  ("presets: h HuggingFace · z Zen · g Go · o Ollama · l LM Studio") —
  one line no longer fits 100–118 columns with the new keys. Two
  existing band-math tests were updated for the fifth button row and the
  second hint line (`f2_providers_scroll_tests`, `w5e_oauth_card_tests`).

## Laws (all by name)

- LK1: `lk1_keyless_profile_resolves_placeholder_and_stored_key_wins`,
  `lk1_keyless_fallback_stays_scoped_to_enabled_auth_none_profiles`
  (daemon accounts_tests), `lk1_keyless_placeholder_bearer_reaches_the_wire_header`
  (provider openai_tests).
- LK2: `lk2_keyless_preset_configure_persists_and_mock_discovery_flips_available`.
- LK3: `lk3_custom_origin_matrix_allows_rfc1918_and_keeps_metadata_and_public_http_blocked`
  (literal matrix + builtin pinned),
  `lk3_custom_lan_hostname_resolution_matrix_pins_both_directions`
  (resolved-address matrix), `lk3_catalog_backstop_obeys_the_custom_lan_matrix`.
  Builtin strictness additionally pinned unchanged by the pre-existing
  `compatible_origin_policy_rejects_credential_ssrf_and_accepts_safe_origins`,
  `hostname_resolution_rejects_every_forbidden_answer_before_bearer_construction`,
  and `plain_http_hostname_requires_every_resolved_address_to_be_loopback`.
- LK4–LK9: `lk4_chat_stream_missing_done_sentinel_completes_on_eof`,
  `lk5_chat_stream_ignores_sse_comment_ping_lines`,
  `lk6_chat_stream_without_usage_still_completes`,
  `lk7_chat_tool_calls_without_ids_synthesize_stable_per_index_ids`,
  `lk8_chat_finish_stop_with_tool_calls_still_completes_the_calls`,
  `lk9_chat_unknown_extra_fields_are_ignored`.
- LK10 (`tests/g4a_local_oss_presets_tests.rs`):
  `ollama_preset_prefills_the_local_origin`,
  `lmstudio_preset_prefills_the_local_origin`,
  `keyless_commit_skips_the_key_card_and_chains_discovery`,
  `footer_hints_and_add_buttons_offer_the_local_presets`,
  `empty_keyless_discovery_hints_start_the_server_then_refresh`.

## Goldens / wire

No new wire families, adapters, request types, or protocol variants.
Presets ride the existing `provider.configure` (`auth_requirement: none`
was already on the wire — `ProviderAuthRequirementWire::None`), so no new
rpc transcript. New request-shape golden: the LK1 placeholder-bearer
header equality. The chat request payload is unchanged (`stream_options`
retention pinned by the existing lingua-franca golden).

## Ledger

`cargo run -p xtask -- test-count --update`: 1940 → 1958 (+18: 3 LK3,
6 LK4–9, 3 LK1, 1 LK2, 5 LK10).

## Deviations

- The brief located the keyless arm at the factory fallthrough
  (accounts.rs:5127-5135). The real gap was twofold: the factory arm AND
  resolution (nothing could ever reach the factory without a stored
  credential). Both shipped; the generic profile arm was re-guarded
  (`!auth_methods.is_empty()`) so the keyless arm is mutation-observable
  rather than shadowed by the pre-existing family arm.
- The footer hint became two lines instead of one (width honesty at
  100–118 columns); the row hint names the concrete key —
  "start the server, then refresh (f)" — since a bare "refresh" named no
  affordance. `f` (models re-discovery for the selected provider) was
  added to make the hint true; it maps to the existing
  `provider.models_refresh` read.
- `open_custom_edit` now derives `keyless` from the summary — without it,
  editing an auth-None profile would submit an `api_key` identity and be
  refused by `require_matching_identity` (create-only identity law).
