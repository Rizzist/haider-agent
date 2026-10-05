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
`crates/haider-provider/src/model_limits.rs`. The daemon projects the table's
context window into `ModelDetailWire.context_window` for every static, remote
and configured row whose catalog declares none (Anthropic, Gemini and DeepSeek
lists carry no window), so automatic compaction and the TUI meter have a real
window. Anthropic's 1M windows (Fable 5.x, Opus 5.x, Opus 4.6-4.8, Sonnet 5,
Sonnet 4.6) are the documented default with no beta header
([context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows),
checked 2026-09-25); Haiku 4.5 is 200K. This is also the source consumed
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

OpenAI OAuth uses the Codex Responses-Lite transport. Its maintained picker
list is limited to the six verified Lite-capable GPT-5.6 and GPT-6 variants;
GPT-5.5 and other standard Responses models are excluded even when present in
the general OpenAI model reference. Remote rows pass the same endpoint check.

Static subscription rows describe the provider and remain available after an
account switch when the new active credential is healthy (`ok`, or `limited`
during a temporary quota window; expired, revoked and attention-needing
credentials make the rows visible but unselectable). Fetched rows belong
only to the account that fetched them. Switching accounts clears those live
rows before the next refresh, so a failed refresh for the new account shows
no rows rather than the previous account's rows as stale; replacing an
inactive alias leaves the active account's rows alone. Removing or re-keying
an account prunes its durable row, and provider removal prunes its remaining
account rows. The prune is best-effort garbage collection of rows no reader
uses; a failed sweep is logged and retried on the next mutation or startup.
A configure that re-submits a provider's unchanged default model is accepted
while the new account's inventory is still unknown.

Vertex (gcloud) keys its catalog by the active gcloud account
(`gcloud config get-value account`), not the rotating access token, so a
same-account re-import keeps its cache.

Authenticated fetched rows are keyed to the descriptor's stable account ID,
then email, then display identity. If an OAuth refresh later adds an account
ID or changes the email without a user account switch, the key changes: a
restart loads a cold catalog and an in-flight refresh can return Busy once.
This identity drift does not expose the previous account's fetched rows.
