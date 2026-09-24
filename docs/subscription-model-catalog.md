# Subscription model fallback catalog

Subscription accounts can run inference even when their credential receives
`403` from `/models`. The maintained IDs live in
`crates/haider-provider/src/subscription_catalog.rs`
(`subscription_static_models`). The daemon merges these with remote
discovery in `provider_registry::merged_catalog_rows`; remote metadata wins
for the same ID, including a remote hidden flag. Every static, remote and
configured row then passes the single `servable_row` gate before projection.
`ModelDetailWire.source` identifies each pickable row as `static`, `remote`,
or `configured`. A failed model-list fetch remains in the provider inventory
as information and does not change account health.

The per-model context and safe output request ceilings live in
`crates/haider-provider/src/model_limits.rs`. This is also the source consumed
by the output-cap lane; do not create another limit table in a client. A
provider that publishes no maximum output limit gets a conservative local
ceiling rather than an invented API maximum.

Snapshot checked 2026-09-24:

| Provider | ID reference | Limit reference |
| --- | --- | --- |
| Anthropic OAuth | [Claude model IDs](https://platform.claude.com/docs/en/models/overview) | Same model table; Fable/Opus/Sonnet 1M and 128K, Haiku 200K and 64K |
| OpenAI OAuth | [OpenAI models](https://developers.openai.com/api/docs/models) | Model cards, including [GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| Kimi OAuth | [Kimi Code model IDs](https://www.kimi.com/code/docs/en/kimi-code/models.html) | Same model table; `k3` is conservatively 256K because 1M depends on plan |
| Grok OAuth | [xAI models](https://docs.x.ai/developers/models) | [Grok 4.6](https://docs.x.ai/developers/models/grok-4.6) and xAI model table |
| Haider Code | [Public `/v1/models` response](https://haidercode.ai/v1/models) checked without credentials on 2026-09-24 | `deepseek-v4-flash` declares 128K context; safe local output ceiling |

Refresh this list when provider model IDs or entitlement rules change. A
static row does not guarantee that every subscription tier can invoke it;
the actual inference response remains the entitlement authority.
