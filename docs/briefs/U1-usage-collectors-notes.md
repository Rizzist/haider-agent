# U1 — cross-provider usage collectors (daemon half) — notes

Lane U1, branch `u1-usage-collectors` @ v0.0.69 (4afc8d3). Scope: per-provider
usage meter collectors, local per-account stats, the `usage.report` RPC behind
`FEATURE_USAGE_REPORT_V1`, and the OpenCode Zen/Go custom-provider presets.
No `/usage` UI — U2 owns the TUI surface.

## Research of record (endpoints, verified)

- **codex/openai-oauth** — `GET https://chatgpt.com/backend-api/wham/usage`,
  `Authorization: Bearer <access token>`. Shape pinned field-for-field from
  the codex ecosystem's own decoders: steipete/CodexBar
  `Sources/CodexBarCore/Providers/Codex/CodexOAuth/CodexOAuthUsageFetcher.swift`
  (`plan_type`, `rate_limit.primary_window`/`secondary_window` each
  `{used_percent, reset_at (epoch s), limit_window_seconds}`,
  `additional_rate_limits[] {limit_name, metered_feature, rate_limit}`,
  `credits`) and luisleineweber/usagebar `docs/providers/codex.md` (sample
  payload). Account email/plan ride the JWT claims: top-level `email`
  (fallback `https://api.openai.com/profile`.email) and
  `https://api.openai.com/auth`.`chatgpt_plan_type` — verified against
  openai/codex `codex-rs/login/src/token_data.rs`. The stored OAuth bundle
  deliberately drops the id_token (haider-accounts oauth.rs), so the daemon
  decodes the ACCESS token's payload (unverified, display-only) — the same
  claims namespace, same flat decode the codex CLI uses. Cache floor 60 s.
  The local openai-oauth account is EXPIRED — parse-fixture coverage only,
  per the brief.
- **anthropic-oauth** — `GET https://api.anthropic.com/api/oauth/usage`,
  Bearer + `anthropic-beta: oauth-2025-04-20` + mandatory
  `User-Agent: claude-code/<ver>`. Shape captured LIVE from the real Claude
  Max account on this machine, 2026-08-05, HTTP 200 (token redacted at
  capture; the body has no secret) — frozen as
  `crates/haider-provider/tests/fixtures/usage/anthropic_oauth_usage_live.json`.
  Buckets `five_hour`/`seven_day`/`seven_day_opus`/`seven_day_sonnet`
  (nullable) + `extra_usage`; RFC 3339 `resets_at`. **Scale dispute is
  real**: the live capture reports PERCENT (`"utilization": 60.0`, with
  `limits[].percent: 60` corroborating) while other public integrations
  report the 0–1 fraction — both scales are law-pinned through one
  normalizer. Cache floor 180 s (the endpoint aggressively 429s tight
  pollers — anthropics/claude-code#31637).
- **kimi-oauth** — `GET https://api.kimi.com/coding/v1/usages`, Bearer.
  Shape from steipete/CodexBar `docs/kimi.md` + usagebar
  `docs/providers/kimi.md`: quota counters are JSON STRINGS
  (`"limit": "2048"`), `limits[]` windows carry
  `{duration, timeUnit: "TIME_UNIT_MINUTE"}`, ISO 8601 `resetTime`. Floor
  300 s (no documented bound; conservative).
- **OpenCode Zen/Go** — `https://opencode.ai/zen/v1` and
  `https://opencode.ai/zen/go/v1`, openai_chat_completions family, Bearer
  `OPENCODE_API_KEY`. `GET {base}/models` verified live 2026-08-05
  unauthenticated (both rosters returned). **No usage endpoint exists**:
  `/zen/v1/usage`, `/balance`, `/credits` all 404 (verified 2026-08-05);
  upstream feature request anomalyco/opencode#10448 (open, assigned) tracks
  a balance API. Seam comment sits in `haider-provider/src/usage.rs`.
- **Pricing table** — `haider-provider/src/pricing.rs`, snapshot
  2026-08-05: Anthropic platform pricing docs, OpenAI pricing after the
  2026-07-30 cut, Google Gemini pricing; corroborated via OpenRouter /
  BenchLM / pricepertoken where vendor pages were unreachable from this
  environment. Longest-prefix match over the normalized id; unknown model →
  `None`, never an invented rate.

## Shipped

- `crates/haider-provider/src/usage.rs` — `UsageMeterEndpoint`
  (url/headers/poll-floor/parse), three pure parsers over recorded bytes,
  `normalize_utilization` (both scales, clamped, total), dependency-free
  RFC 3339 → Unix-ms parser, typed `MeterUnavailable`. New fixtures under
  `tests/fixtures/usage/` (live capture + fraction-scale variant + wham +
  kimi). `src/pricing.rs` — the static table + `estimate_chunk_cost_usd`
  (reasoning bills as output; cache reads at the cache rate, else input
  rate).
- `crates/haider-protocol/src/usage.rs` — `UsageReportV1`,
  `AccountUsageReportV1`, tagged `AccountMeterStateV1`
  (`metered`/`unavailable`/`local_only`), `UsageWindowV1`
  (utilization ALWAYS the 0–1 fraction on the wire, `resets_at_ms`),
  `LocalUsageStatsV1`. New goldens `usage_report_v1` +
  `usage_meter_unavailable`; existing fixtures untouched.
- `crates/haider-rpc` — `FEATURE_USAGE_REPORT_V1` (`usage_report_v1`),
  parameterless `RequestBody::UsageReport` (`usage.report`),
  `ResponseBody::UsageReport { report }`. Golden transcript: U1 appends
  exactly three frames at the END (`UPDATE_FIXTURES=1` regen; earlier bytes
  byte-frozen — the D1 "last wave" fence in
  `device_discovery_goldens_are_additive_and_tolerance_re_proved` was
  re-anchored to the U1 welcome, preserving its exact six-frame law).
- `crates/haider-daemon/src/usage_report.rs` — `UsageReportService`
  installed on the hub (runtime shares the SAME `CredentialBroker` as
  provider construction via the `MeterTokenSource` seam; HTTP behind
  `UsageMeterHttp` with a pinned-policy reqwest production impl). Per-account
  meter cache: floors enforced, failures cached as typed unavailability
  (never hammering, never stale-good resurrection), secrets never in
  reasons or cache. Local accounting: exact incremental `SessionFolder`
  per session — last cumulative `Usage` snapshot per `(run, agent)`,
  `model_selected` tracked in seq order for pricing, LOC from COMPLETED
  `fs_write`/`fs_patch`/`fs_edit` receipts
  (content / preimage→replacement / old→new line counts), unattributed
  usage skipped; session count, span, and LOC attribute to the dominant
  account, token totals exactly per usage events. Dispatch arm authorizes
  `View`; a daemon without the installed service answers an honest empty
  report (the `account.list` missing-facade precedent).
- `crates/haider-tui` — `AccountAddKind::OpencodeZen`/`OpencodeGo`, the
  shared `open_custom_preset` seam (HF refactored onto it), keys `z`/`g`
  on /providers, add-rows on /accounts, hint + help text. Two bottom-band
  geometry laws re-anchored for the fourth button row (same strictness,
  new footer height).
- `crates/haider-daemond/tests/usage_report_rpc_tests.rs` — wire law over a
  real UnixStream; `crates/haider-provider/tests/usage_live_tests.rs` —
  gated live poll (ignored by default).

## Laws (ledger 1707 → 1731, +24)

haider-protocol (+2): `golden_usage_report_v1`,
`usage_report_fields_are_tolerant_and_additive`.
haider-rpc (+1):
`usage_report_goldens_are_additive_normalized_and_secret_free` (append-only
tail, normalized fraction on the wire, secret-key sweep, both-direction
tolerance).
haider-provider (+10):
`openai_wham_fixture_yields_primary_secondary_and_named_extra_windows`,
`anthropic_live_fixture_normalizes_percent_scale_and_rfc3339_resets`,
`anthropic_fraction_fixture_reads_identically_to_the_percent_scale`,
`kimi_fixture_reads_string_counters_and_names_rolling_windows`,
`normalize_utilization_accepts_both_scales_and_clamps`,
`failures_are_typed_unavailable_never_a_fabricated_reading`,
`endpoint_coordinates_headers_and_poll_floors_are_pinned`,
`rfc3339_parser_is_exact_to_the_millisecond_and_total`,
`pricing_estimates_known_families_and_refuses_unknown_models`,
`live_anthropic_oauth_meter_parses_and_normalizes` (gated live).
haider-daemon (+6):
`api_key_and_custom_accounts_are_local_only_and_never_probe_http`,
`oauth_meter_reading_normalizes_and_respects_the_poll_floor`,
`meter_failures_are_typed_cached_and_never_hammered`,
`openai_token_claims_supply_email_and_plan_with_meter_precedence`,
`session_folder_attributes_tokens_cost_duration_and_loc`,
`meter_routing_is_flavor_and_provider_strict`.
haider-daemond (+1): `usage_report_is_advertised_and_answers_typed_over_uds`.
haider-tui (+4): `opencode_zen_preset_prefills_the_zen_gateway`,
`opencode_go_preset_prefills_the_go_gateway`,
`presets_gate_on_the_daemon_configure_feature`,
`accounts_add_rows_offer_both_presets`.

## Executed mutation campaign — 8/8 kills (1 survivor closed)

Protocol per kill: apply, run the named test ("running 1 test" observed),
record the runtime failure, revert, re-run green.

1. **Mutation:** `normalize_utilization` percent branch removed (any value
   above 1 clamps straight to 1.0) — `haider-provider/src/usage.rs`.
   KILLED by `anthropic_live_fixture_normalizes_percent_scale_and_rfc3339_resets`:
   `assertion failed: (reading.windows[0].utilization - 0.6).abs() < 1e-9`.
   Reverted.
2. **Mutation:** wham `used_percent` passed to the normalizer without `/100`
   — `haider-provider/src/usage.rs`. **SURVIVED the original law** (the
   defensive normalizer rescues every integer percent) — honest gap: only
   `used_percent ∈ (0, 1]` distinguishes the seams. Law strengthened with a
   `Sub-Percent-Lane` window (`used_percent: 0.5` → 0.005) and the mutation
   re-executed: KILLED by
   `openai_wham_fixture_yields_primary_secondary_and_named_extra_windows`:
   `assertion failed: (reading.windows[3].utilization - 0.005).abs() < 1e-9`.
   Reverted.
3. **Mutation:** RFC 3339 offsets ignored (every `±HH:MM` treated as `Z`) —
   `haider-provider/src/usage.rs`. KILLED by
   `rfc3339_parser_is_exact_to_the_millisecond_and_total`:
   `left: Some(1785943200000), right: Some(1785923400000)` ("offset
   arithmetic lands on the same instant"). Reverted.
4. **Mutation:** meter cache floor dropped (`&& false` on the cache hit) —
   `haider-daemon/src/usage_report.rs`. KILLED by
   `oauth_meter_reading_normalizes_and_respects_the_poll_floor`:
   `left: 2, right: 1` ("the poll floor forbids a refetch"). Reverted.
5. **Mutation:** cumulative usage snapshots SUMMED per (run, agent) instead
   of last-wins — `haider-daemon/src/usage_report.rs`. KILLED by
   `session_folder_attributes_tokens_cost_duration_and_loc`:
   `left: 2600000, right: 2200000` (double count). Reverted.
6. **Mutation:** cache reads billed at the full input rate —
   `haider-provider/src/pricing.rs`. KILLED by
   `pricing_estimates_known_families_and_refuses_unknown_models`:
   `cost 5.85 != expected 5.31`. Reverted.
7. **Mutation:** Go preset pointed at the Zen origin (the plausible
   copy-paste) — `haider-tui/src/app.rs`. KILLED by
   `opencode_go_preset_prefills_the_go_gateway`:
   `left: "https://opencode.ai/zen/v1", right: "https://opencode.ai/zen/go/v1"`.
   Reverted.
8. **Mutation:** auth-method guard dropped from `meter_for` (API-key
   descriptors route to OAuth meters) — `haider-daemon/src/usage_report.rs`.
   KILLED by `meter_routing_is_flavor_and_provider_strict`:
   `left: Some(OpenAiOauth), right: None` ("api-key openai-oauth must never
   meter"). Reverted.

Full crate suites green after every revert.

## Live verify (final clean build)

`HAIDER_LIVE_USAGE_TESTS=1` + the machine's real Claude Max access token
(read from the OS credential store into env, never printed), 2026-08-05:

```
live window five_hour utilization=0.830 resets_at_ms=Some(1785923399439)
live window seven_day utilization=0.170 resets_at_ms=Some(1785963599439)
test result: ok. 1 passed
```

A genuinely fresh reading (moved from the earlier 0.60/0.12 capture while
this lane consumed the window) — percent-scale live truth normalized to the
0–1 wire fraction by the real parser + header set.

## Gate

`cargo fmt --all` clean; workspace clippy: no `unwrap/expect` or other
warnings (all-targets); full workspace test run green
(`CARGO_INCREMENTAL=0`, `ulimit -n 8192`); wire transcript regenerated with
`UPDATE_FIXTURES=1` (append-only); ledger
`cargo run -p xtask -- test-count --update` → **1731** in
`test-baseline.txt` is the truth. No version bumps, no tags, Cargo.lock
untouched.

## Not in this wave / honest limits

- U2 owns the `/usage` TUI; nothing renders the report yet.
- Local stats join per KNOWN descriptor alias: usage recorded under a
  since-removed account drops out of the report (derived data, not truth
  loss — the journal keeps it).
- The journal scan is on-demand per `usage.report` (no rollup table); fine
  at dev-profile scale, a v12 migration seam if it ever isn't.
- `fs_edit` under `replace_all` counts one occurrence (the receipt carries
  no occurrence count); `fs_write` counts added lines only (prior contents
  unknown at the receipt).
- Session/duration/LOC attribution is dominant-account (most tokens in the
  session); token totals stay exact per account.
- kimi-oauth meter is fixture-verified only (no live kimi account on this
  machine); openai-oauth likewise (account expired) — both parsers ride
  shapes lifted from their ecosystems' shipping decoders.
