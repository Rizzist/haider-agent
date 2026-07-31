# W5g-5b — custom providers actually serve: family-routed turns + {origin}/models discovery

The W5g-4/5a card creates a custom provider profile (family
`openai_chat_completions`, origin, seeded model+default) and chains into
the key card. Two daemon gaps keep it from ever serving a turn:

1. **Turn routing is name-keyed, not family-keyed.**
   `build_account_provider` (`crates/haider-daemon/src/accounts.rs`)
   routes `(OPENAI_COMPATIBLE_PROVIDER_NAME, ApiKey)` to
   `OpenAiCompatibleProvider` with the CREDENTIAL's base_url. A custom
   profile named `custom-llama` falls to the `_` arm ("no account-backed
   adapter"). Fix: resolve the provider PROFILE (registry lookup — the
   caller path has access to the registry; plumb what is needed) and
   route ANY profile with `api_family == OpenAiChatCompletions` +
   `AuthMethod::ApiKey` to `OpenAiCompatibleProvider` against the
   PROFILE's `base_url` (the credential's own base_url wins only when the
   profile has none — the legacy `openai-compatible` fixed-name path must
   keep working byte-for-byte).

2. **No model discovery for custom origins.** `catalog_source()`
   (`accounts.rs`) maps only the two OAuth vendor sources; a custom
   provider's `provider.models_refresh` answers "unavailable". Fix: when
   the provider resolves to a custom chat-completions profile, discover
   from `GET {origin}/models` (openai-compat shape:
   `{"object":"list","data":[{"id":...}]}` — `id` is the slug AND the
   display name; `context_window` is NOT declared → `None`, never a
   guess; all models visible, no effort ladders, no priority). Auth: if
   the refresh runs under an api-key credential for that provider, send
   `Authorization: Bearer <key>`; with `auth_requirement none` send no
   auth header.

## Security laws (non-negotiable)

- The fetch target is ALWAYS the STORED profile origin — never a
  client-supplied string at refresh time.
- Validate the origin BEFORE fetching (discovery-time backstop —
  configure-time has no URL policy today): scheme http or https only;
  `http` permitted ONLY for loopback hosts (127.0.0.0/8, ::1,
  localhost); no userinfo, no fragment. Redirects DISABLED; response
  body BOUNDED (reuse the existing catalog byte limits); connect/read
  timeouts per the existing catalog transport.
- The api key rides only as the auth header of this one request; it must
  never appear in errors, logs, or the unavailable-reason string.

## Where things live

- `crates/haider-provider/src/catalog.rs` — `CatalogSource` is a Copy
  enum with 'static vendor endpoints; a custom origin is dynamic. Design
  freedom is yours (a new source variant carrying the origin, or a
  separate entry fn reusing `parse_catalog`'s plumbing) as long as the
  vendor paths stay byte-identical and `parse_catalog` gains the
  openai-compat shape with its own tests.
- `crates/haider-daemon/src/accounts.rs` — `catalog_source()`,
  `begin/finish_provider_models_refresh`, `build_account_provider`.
- `crates/haider-daemon/src/provider_registry.rs` — profile lookup
  (`ProviderProvenance::Custom`, `base_url`).

## Laws

- Tests NEVER inline — `tests/` dirs or `*_tests.rs` sibling modules.
- Every law-bearing test documents its mutation + expected RUNTIME
  failure.
- `CARGO_INCREMENTAL=0` on every cargo invocation; finish with
  `cargo fmt --all`, workspace clippy `-D warnings` clean, and
  `CARGO_INCREMENTAL=0 cargo test -p haider-provider -p haider-daemon`
  (your sandbox cannot bind sockets — loopback-binding test failures are
  expected there; the orchestrator's host gate is authoritative).
- Update the ledger: `CARGO_INCREMENTAL=0 cargo run -p xtask --
  test-count --update`.
- Do NOT touch haider-tui, haider-rpc wire shapes, Cargo.lock, versions.
- Leave all changes uncommitted; do NOT git commit/checkout/reset.

## Tests (minimum)

- provider: openai-compat catalog parse — `data[]` ids become models,
  absent windows stay `None`, malformed → unavailable-not-substituted.
  (Mutation: parse arm fabricates a window → test fails.)
- provider or daemon: origin validation — remote `http://` rejected
  BEFORE any fetch; `https://` remote allowed; loopback http allowed.
  (Mutation: loopback check removed → remote-http test fails.)
- daemon: a custom profile routes to `OpenAiCompatibleProvider` with the
  PROFILE origin (construction-level assert); the fixed-name legacy path
  still routes with the credential base_url. (Mutation: family arm
  removed → custom-profile test fails with "no account-backed adapter".)
- daemon: `models_refresh` for a custom profile publishes the discovered
  slugs in the summary (stub transport). (Mutation: custom arm dropped →
  refresh answers unavailable.)

Use up to 2 research subagents and 1-2 verify subagents before you
finish. Print a final summary of files changed and tests added.
