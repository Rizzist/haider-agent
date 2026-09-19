# Model inventory provenance

Remote catalogs must not ship fallback inventories. `ProviderCatalogDefinition`
in `haider-provider` distinguishes offline catalogs, public endpoints,
authenticated endpoints, and adapter-owned inventories. Only the offline
variant has a static-model field. Haider Code is public even though inference
still requires an API key. DeepSeek and xAI also no longer seed model rows.

The daemon cache stores one `ProviderInventory` value under one lock. A missing
fetch cannot carry rows; successful fetches carry their time, failures without
a prior fetch carry a reason, and failures after a fetch retain the last known
rows as stale. Cache age is converted to stale state at snapshot boundaries.
Selection and refresh use provenance, not a nonempty-list/optional-age proxy.

A fresh Haider Code profile has no models and no default. Enter on its empty
picker row requests discovery. Public discovery never resolves or sends a
credential, including when a caller supplies an invalid key. After discovery,
Haider Code's automatic default is selected from the returned pickable IDs.
Persisted Go/Go Max defaults are cleared on upgrade. They are not aliases.
Ordinary and resumed turns recheck persisted session models against the same
selection authority before resolving credentials. Adapter construction and
attempt resolution also enforce admission for refresh, rotation and retries.
An obsolete saved selection receives a nonretryable refusal with refresh and
explicit reselection instructions; it is never silently replaced by a default
or normalized alias. A stale remote catalog can still admit its listed IDs.
Offline catalogs and explicitly user-configured custom advisory routes retain
their separate semantics; a user-owned passthrough ID is not advertised as a
discovered model.

`ProviderSummaryWire` now carries `catalog` and tagged `inventory` values.
Flat `models`, `model_details`, and `default_model` remain compatibility
projections. The ambiguous `inventory_fetched_at_ms` field is replaced by the
fetched/stale variant's timestamp. Older clients ignore the additive fields
and can still list models; newer clients treat summaries without provenance as
never fetched, rather than inferring discovery from old rows. Wire goldens
record this intentional provider-summary shape change.

`haider models --refresh` collects errors per provider. One expired OAuth
credential leaves other providers' rows intact; failed providers expose their
own reason, and a previous successful catalog remains stale rather than empty.

The ordinary gate checks taxonomy, unfetched/fetched/stale/unavailable
transitions, default membership, credential-free request construction,
provider-scoped failures, and the picker fetch affordance. The scheduled
`public catalog drift` workflow runs the ignored `public_catalog_drift` probe
without secrets. It checks that public endpoints return a usable catalog and
that every shipped static ID is a subset of the live IDs. Under the current
taxonomy the public static set is empty by construction; the hermetic test
pins that law. Offline tables have no live endpoint to compare. Network
availability and upstream catalog changes belong to the nightly tier instead
of making PR tests depend on external services.

Local CLI proof uses `HAIDER_DISCOVERY_DISABLED=1`, a new `HAIDER_PROFILE_DIR`
and a private `HAIDER_RUNTIME_DIR`: first `haider provider list` and
`haider models --json`, then `haider models --refresh haider-code --json`.
The first view must show never-fetched/no default; the second must contain
actual public IDs. Explicit Go selection must refuse. Stop only that profile's
daemon afterward with `haider daemon stop`.

The repair regression imports actual session/journal rows created by the
installed 0.0.971 CLI into a fresh store and resumes them through the candidate
CLI. The paired credentialed recording-provider test uses the same release
metadata and asserts zero provider rounds for ordinary and resumed turns.
Its mutation removes persisted-model admission and must send Go to the
recording provider. Fixture provenance and release hashes are recorded in
`crates/haider-cli/tests/fixtures/preupgrade-go-session.md`.
