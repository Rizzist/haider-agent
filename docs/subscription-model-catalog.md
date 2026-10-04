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
Fetched defaults and picker selections are inventory too. The daemon persists
their serving catalog coordinate and clears them when the incarnation,
endpoint or auth mode changes; explicit configured deployments and static
release-owned IDs remain provider configuration. The CLI does not turn a
summary's derived default into an explicit configure request, and renders
intake from the post-commit view. The TUI likewise resolves automatic session
defaults inside the daemon's create request; a catalog picker choice carries
its observed producer key. Create and select refuse a delayed choice from a
retired producer. Durable session metadata, selection events/receipts,
headless run pins, delegation and automatic pair switches retain that key. A
turn checks it against both the current profile and the actual credential being used, so
an advisory custom endpoint cannot reuse a former account's derived default.
Attached sessions await explicit reselection; the launcher can adopt the new
account's default. Explicit unlisted input and legacy metadata without a
catalog owner retain their configuration semantics. A permanent model ID can
still have account-owned fetched settings: deriving a session's output default
from remote metadata, or clamping a user budget to a fetched ceiling, captures
that metadata's producer, even for a configured
deployment or static ID. Provider summaries retain declared deployments
separately from fetched details. Retiring a producer preserves those declared
names while clearing fetched capability, context, effort and speed fields.
Automatic switches reapply the stored budget source to the target model before
persisting its selection and issuing requests; recovery retains that budget
with the switch's producer.
Configuration-only custom updates preserve omitted explicit deployments and
same-incarnation selections. The TUI retires current markers and launcher
defaults on an incarnation change. The last-model preference persists its
producer, preserves it across theme writes and sends it even before the first
inventory reply after restart. Retiring a pick removes the preference too.
Legacy preferences with unknown ownership defer to daemon resolution until
fresh inventory proves a permanent choice; explicit unowned configuration is
recorded distinctly and keeps its spelling. Provider summaries carry an opaque catalog
producer key and its account incarnation, so independently arriving account
and provider replies cannot adopt an old default. A newer catalog waits for
its matching account reply; a retired catalog loses fetched rows and
capability metadata even if an unrelated provider has advanced the global
management revision. Adapter capability caches include the
incarnation, and automatic failover/promotion require the current model list
and the selection's original producer, including anonymous endpoint/auth
context. Pending default-model receipts carry the same owner; retired receipts
cannot restore a fetched selection. Recovered
discovery rows remain fetched inventory rather than permanent deployments.

Vertex (gcloud) keys its catalog by the active gcloud account
(`gcloud config get-value account`), not the rotating access token, so a
same-account re-import keeps its cache.

Authenticated fetched rows use a v3 cache key containing the provider, alias,
and durable account incarnation. Each profile reserves monotonically
increasing incarnations in a separate fsynced counter before saving a
descriptor; remove/re-add cannot recycle one, including after a crash. The
v2 identity/epoch cache is pruned on migration, causing one cold fetch.
Neither login time, source mtime, a display label, nor a source path establishes
account identity.

Intake keeps an incarnation only for identical ingested bytes in the same
provider/auth/origin context, or an unchanged complete provider-issued user
coordinate. A workspace ID alone does not prove an OpenAI user; source IDs,
truncated fields and unverified display metadata do not prove continuity.
Otherwise import/re-login/repoint/auth changes mint before publication, clear
live rows and derived selections, fence late completions by the complete
producer, and prune unreachable durable rows. A failed prune cannot authorize
a read under the new key. Inactive replacements do not clear active inventory.

Owned grant-only Kimi, Grok and Anthropic identities survive OAuth token
rotation and manual refresh when their grant coordinate is unchanged. New
intakes still use the stricter rule. Linked-source partial/unreadable writes
retain the last digest, keep attention status, and compare recovered bytes;
source removal retires the incarnation. A discovery aborted by source rotation
reconciles and requeues once the source is readable. Source enrollment, scans
and reconciliation publish account and provider views together.

Configured deployments share one availability policy across every provider kind.
Before the first model-list attempt, a provider that requires supported discovery
is Unavailable; this includes Azure. Definite negative evidence also makes a
selection Unavailable: credential/authentication failure, a deployment absent
from a successful fresh list, or a disabled/removed provider.

Uncertainty keeps configured rows selectable with a visible reason. A previous
successful list remains usable after its 15-minute freshness window, and 503 or
timeout results remain selectable even on a first attempted fetch. HTTP 404/405
or a typed unsupported listing shows **configured (unverified)**. That unverified
inventory remains excluded from automatic failover and promotion. Maintained
offline and OAuth catalogs retain their permanent rows with healthy credentials.

Staleness schedules background refresh with bounded backoff; it never blocks
new sessions or turns of existing sessions. The daemon's admission, CLI, TUI
picker and summaries use `ProviderSummaryWire::selection_availability` and its
shared reason projection. A fresh empty list does not authorize configured rows.
A stale list preserves fetched metadata and may show missing explicit configured
rows without borrowing metadata from another inventory.

The running TUI refreshes management state during idle. Retiring a catalog
producer clears every current identity projection, including the launcher and
session headers, composer, account label and context meter. A restored private
model preference stays hidden until live snapshots prove its owner. Startup may
adopt a legacy chosen default only after loading the active producer's matching
inventory; summary filtering independently rejects foreign defaults.

Route edits preserve a session selection's original catalog key. Changing the
provider, account override, or endpoint does not turn fetched model settings
into explicit configuration. Admission requires a fresh selection for the new
producer, and the TUI retires its current marker immediately. The daemon projects
release-owned IDs separately from remote metadata, so a fetched override cannot
make the permanent name belong to the former account.

Session endpoint/account overrides retire the active route's fetched enrichment
even when the model name is explicit. The shared wire projection removes remote
context, output limits, effort/speed and vision/thinking declarations. Both the
picker and daemon selection authority use it; the authority also removes the old
producer key, so a queued choice cannot become explicit configuration. Unsupported
Azure-listing evidence belongs to its route and cannot authorize another account.
