//! U1 usage-collector laws: parsers over REAL response shapes, defensive
//! utilization normalization, typed unavailability, and the bundled pricing
//! table.
//!
//! Fixture provenance (also cited in `src/usage.rs`):
//! - `usage/openai_wham_usage.json` — assembled from the codex ecosystem's
//!   own decoders: steipete/CodexBar
//!   `Sources/.../CodexOAuth/CodexOAuthUsageFetcher.swift` (field-for-field
//!   `CodingKeys`) and the sample payload in luisleineweber/usagebar
//!   `docs/providers/codex.md`.
//! - `usage/anthropic_oauth_usage_live.json` — captured LIVE from
//!   `GET https://api.anthropic.com/api/oauth/usage` with a real Claude Max
//!   account on 2026-08-05 (HTTP 200; token redacted at capture time; the
//!   body itself carries no secret).
//! - `usage/anthropic_oauth_usage_fraction.json` — the SAME buckets on the
//!   0–1 fraction scale some sources report for this endpoint; the pair
//!   pins the normalizer to one wire scale.
//! - `usage/kimi_usages.json` — sample from steipete/CodexBar `docs/kimi.md`
//!   / luisleineweber/usagebar `docs/providers/kimi.md` (string counters).
#![allow(clippy::expect_used)] // tests may expect; the lint guards src/ only

use haider_protocol::provider::{CacheStatAvailability, NormalizedUsage, ReasoningAccounting};
use haider_provider::{
    ANTHROPIC_OAUTH_USAGE_URL, CacheWriteTtl, KIMI_OAUTH_USAGE_URL, MeterUnavailable,
    OPENAI_OAUTH_ACCOUNT_ID_HEADER, OPENAI_OAUTH_USAGE_ORIGINATOR, OPENAI_OAUTH_USAGE_URL,
    OPENAI_OAUTH_USAGE_USER_AGENT, UsageMeterEndpoint, estimate_cache_input_costs,
    estimate_cache_input_costs_for, estimate_cache_rewarm_cost_usd, estimate_chunk_cost_usd,
    estimate_chunk_cost_usd_for, estimate_normalized_usage_cost_usd_for, model_rate,
    normalize_utilization, parse_rfc3339_to_unix_ms,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/usage")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {name}"))
}

/// LAW (openai_wham_fixture_yields_primary_secondary_and_named_extra_windows):
/// the codex meter parse maps `rate_limit.primary_window`/`secondary_window`
/// to human `5h`/`weekly` windows (percent → 0–1 fraction, `reset_at`
/// seconds → ms), surfaces each `additional_rate_limits[]` entry under its
/// `limit_name` label, and reports the subscription `plan_type` — while
/// tolerating unknown siblings such as `code_review_rate_limit`.
#[test]
fn openai_wham_fixture_yields_primary_secondary_and_named_extra_windows() {
    let reading = UsageMeterEndpoint::OpenAiOauth
        .parse(200, &fixture("openai_wham_usage.json"))
        .expect("wham fixture parses");
    assert_eq!(reading.plan.as_deref(), Some("plus"));
    assert_eq!(reading.windows.len(), 4, "primary, secondary, two named");
    assert_eq!(reading.windows[0].window, "5h");
    assert!((reading.windows[0].utilization - 0.06).abs() < 1e-9);
    assert_eq!(reading.windows[0].resets_at_ms, Some(1_738_300_000_000));
    assert_eq!(reading.windows[0].label, None);
    assert_eq!(reading.windows[1].window, "weekly");
    assert!((reading.windows[1].utilization - 0.24).abs() < 1e-9);
    assert_eq!(reading.windows[1].resets_at_ms, Some(1_738_900_000_000));
    assert_eq!(reading.windows[2].window, "5h");
    assert_eq!(
        reading.windows[2].label.as_deref(),
        Some("GPT-5.3-Codex-Spark")
    );
    assert!((reading.windows[2].utilization - 0.41).abs() < 1e-9);
    // Sub-percent window: `used_percent` is percent BY NAME — 0.5 means
    // 0.5%, and only the parser's own /100 lands it at 0.005 (the defensive
    // normalizer alone would misread 0.5 as a 50% fraction).
    assert_eq!(
        reading.windows[3].label.as_deref(),
        Some("Sub-Percent-Lane")
    );
    assert!((reading.windows[3].utilization - 0.005).abs() < 1e-9);
}

/// LAW: OpenAI plans may publish either subscription window independently.
/// Relative resets are anchored to the fetch instant, and an absent/null
/// primary window never suppresses a valid weekly window.
///
/// MUTATION CHECK: require `primary_window` before examining secondary; the
/// secondary-only case changes from one weekly window to an error.
#[test]
fn openai_optional_window_matrix_keeps_every_window_that_exists() {
    const FETCHED_AT_MS: u64 = 1_800_000_000_000;
    let secondary = UsageMeterEndpoint::OpenAiOauth
        .parse_at(
            200,
            &fixture("openai_wham_secondary_only.json"),
            FETCHED_AT_MS,
        )
        .expect("secondary-only fixture parses");
    assert_eq!(secondary.windows.len(), 1);
    assert_eq!(secondary.windows[0].window, "weekly");
    assert!((secondary.windows[0].utilization - 0.08).abs() < 1e-9);
    assert_eq!(
        secondary.windows[0].resets_at_ms,
        Some(FETCHED_AT_MS + 604_800_000)
    );

    let primary = UsageMeterEndpoint::OpenAiOauth
        .parse_at(
            200,
            &fixture("openai_wham_primary_only.json"),
            FETCHED_AT_MS,
        )
        .expect("primary-only fixture parses");
    assert_eq!(primary.windows.len(), 1);
    assert_eq!(primary.windows[0].window, "5h");
    assert_eq!(
        primary.windows[0].resets_at_ms,
        Some(FETCHED_AT_MS + 7_200_000)
    );

    assert_eq!(
        UsageMeterEndpoint::OpenAiOauth.parse(200, &fixture("openai_wham_empty.json")),
        Err(MeterUnavailable::new("no_windows_reported"))
    );
    assert_eq!(
        UsageMeterEndpoint::OpenAiOauth.parse(401, &fixture("openai_wham_401.json")),
        Err(MeterUnavailable::new("http_status_401"))
    );
    assert_eq!(
        UsageMeterEndpoint::OpenAiOauth.parse(200, &fixture("openai_wham_malformed.json")),
        Err(MeterUnavailable::new("malformed_response"))
    );
}

/// LAW (anthropic_live_fixture_normalizes_percent_scale_and_rfc3339_resets):
/// the LIVE-captured anthropic payload (percent scale: `utilization: 60.0`
/// with `limits[].percent: 60` corroborating) lands on the wire as the 0.6
/// fraction; RFC 3339 `resets_at` becomes exact Unix ms; null buckets and a
/// disabled `extra_usage` yield NO window — absent, never zero-invented.
#[test]
fn anthropic_live_fixture_normalizes_percent_scale_and_rfc3339_resets() {
    let reading = UsageMeterEndpoint::AnthropicOauth
        .parse(200, &fixture("anthropic_oauth_usage_live.json"))
        .expect("live fixture parses");
    assert_eq!(reading.plan, None, "this meter reports no plan");
    assert_eq!(
        reading.windows.len(),
        2,
        "five_hour + seven_day; nulls and disabled extra_usage are absent"
    );
    assert_eq!(reading.windows[0].window, "five_hour");
    assert!((reading.windows[0].utilization - 0.6).abs() < 1e-9);
    assert_eq!(reading.windows[0].resets_at_ms, Some(1_785_923_400_833));
    assert_eq!(reading.windows[1].window, "seven_day");
    assert!((reading.windows[1].utilization - 0.12).abs() < 1e-9);
    assert_eq!(reading.windows[1].resets_at_ms, Some(1_785_963_600_833));
}

/// LAW (anthropic_fraction_fixture_reads_identically_to_the_percent_scale):
/// the SAME utilization reported on the 0–1 scale produces the SAME wire
/// number as the live percent scale (0.6 == 0.6), a full fraction stays 1.0,
/// and an ENABLED `extra_usage` (percent scale) joins as its own window —
/// the 0–1/0–100 disagreement between sources is dissolved by law.
#[test]
fn anthropic_fraction_fixture_reads_identically_to_the_percent_scale() {
    let reading = UsageMeterEndpoint::AnthropicOauth
        .parse(200, &fixture("anthropic_oauth_usage_fraction.json"))
        .expect("fraction fixture parses");
    assert_eq!(reading.windows.len(), 4);
    assert_eq!(reading.windows[0].window, "five_hour");
    assert!((reading.windows[0].utilization - 0.6).abs() < 1e-9);
    assert_eq!(reading.windows[0].resets_at_ms, Some(1_785_923_400_000));
    assert_eq!(reading.windows[2].window, "seven_day_opus");
    assert!((reading.windows[2].utilization - 1.0).abs() < 1e-9);
    assert_eq!(reading.windows[3].window, "extra_usage");
    assert!((reading.windows[3].utilization - 0.25).abs() < 1e-9);
    assert_eq!(reading.windows[3].resets_at_ms, None);
}

#[test]
fn anthropic_seven_day_only_is_a_valid_meter_reading() {
    let reading = UsageMeterEndpoint::AnthropicOauth
        .parse(200, &fixture("anthropic_oauth_seven_day_only.json"))
        .expect("seven-day-only fixture parses");
    assert_eq!(reading.windows.len(), 1);
    assert_eq!(reading.windows[0].window, "seven_day");
    assert!((reading.windows[0].utilization - 0.6).abs() < 1e-9);
}

/// LAW (kimi_fixture_reads_string_counters_and_names_rolling_windows): kimi
/// quota counters arrive as JSON STRINGS and still divide into a fraction
/// (`used/limit`), the plan-level quota surfaces as `quota` with its ISO
/// reset instant in ms, and each `limits[]` entry is named from its window
/// spec (`300 TIME_UNIT_MINUTE` → `window_300m`).
#[test]
fn kimi_fixture_reads_string_counters_and_names_rolling_windows() {
    let reading = UsageMeterEndpoint::KimiOauth
        .parse(200, &fixture("kimi_usages.json"))
        .expect("kimi fixture parses");
    assert_eq!(reading.windows.len(), 2);
    assert_eq!(reading.windows[0].window, "quota");
    assert!((reading.windows[0].utilization - 214.0 / 2048.0).abs() < 1e-9);
    assert_eq!(reading.windows[0].resets_at_ms, Some(1_767_972_193_716));
    assert_eq!(reading.windows[1].window, "window_300m");
    assert!((reading.windows[1].utilization - 139.0 / 200.0).abs() < 1e-9);
    assert_eq!(reading.windows[1].resets_at_ms, Some(1_767_706_382_717));
}

/// LAW (normalize_utilization_accepts_both_scales_and_clamps): the
/// normalizer is total and monotone — fractions pass through, percentages
/// divide by 100, and everything outside [0, 100] (including NaN/∞ and
/// negatives) clamps to an honest bound. The ambiguous 1.0 reads as the
/// FULL fraction (the direction that can only over-report usage).
#[test]
fn normalize_utilization_accepts_both_scales_and_clamps() {
    let cases = [
        (60.0, 0.6),
        (0.6, 0.6),
        (0.0, 0.0),
        (0.005, 0.005),
        (1.0, 1.0),
        (2.5, 0.025),
        (100.0, 1.0),
        (150.0, 1.0),
        (-3.0, 0.0),
        (f64::NAN, 0.0),
        (f64::INFINITY, 1.0),
    ];
    for (raw, expected) in cases {
        let normalized = normalize_utilization(raw);
        assert!(
            (normalized - expected).abs() < 1e-9,
            "normalize({raw}) = {normalized}, expected {expected}"
        );
        assert!((0.0..=1.0).contains(&normalized), "always inside [0, 1]");
    }
}

/// LAW (failures_are_typed_unavailable_never_a_fabricated_reading): a non-2xx
/// status, malformed bytes, or a windowless body each produce a typed
/// `MeterUnavailable` with a bounded snake_case reason — never a panic and
/// never an empty-but-"successful" meter.
#[test]
fn failures_are_typed_unavailable_never_a_fabricated_reading() {
    for endpoint in [
        UsageMeterEndpoint::OpenAiOauth,
        UsageMeterEndpoint::AnthropicOauth,
        UsageMeterEndpoint::KimiOauth,
    ] {
        assert_eq!(
            endpoint.parse(429, b"{}"),
            Err(MeterUnavailable::new("http_status_429")),
            "status beats body for {endpoint:?}"
        );
        assert_eq!(
            endpoint.parse(200, b"not json"),
            Err(MeterUnavailable::new("malformed_response"))
        );
        assert_eq!(
            endpoint.parse(200, b"{}"),
            Err(MeterUnavailable::new("no_windows_reported"))
        );
    }
}

/// LAW (endpoint_coordinates_headers_and_poll_floors_are_pinned): the three
/// meter URLs are the researched literals; anthropic carries the mandatory
/// beta + Claude Code User-Agent headers; OpenAI carries the same account-
/// scoped client identity header set as Codex; and cache floors honor the
/// brief (codex ≥ 60 s, anthropic ≥ 180 s).
#[test]
fn endpoint_coordinates_headers_and_poll_floors_are_pinned() {
    assert_eq!(
        OPENAI_OAUTH_USAGE_URL,
        "https://chatgpt.com/backend-api/wham/usage"
    );
    assert_eq!(
        ANTHROPIC_OAUTH_USAGE_URL,
        "https://api.anthropic.com/api/oauth/usage"
    );
    assert_eq!(
        KIMI_OAUTH_USAGE_URL,
        "https://api.kimi.com/coding/v1/usages"
    );
    assert_eq!(
        UsageMeterEndpoint::OpenAiOauth.url(),
        OPENAI_OAUTH_USAGE_URL
    );
    let openai_headers = UsageMeterEndpoint::OpenAiOauth.extra_headers();
    assert_eq!(OPENAI_OAUTH_ACCOUNT_ID_HEADER, "chatgpt-account-id");
    assert!(openai_headers.contains(&("accept", "application/json")));
    assert!(openai_headers.contains(&("originator", OPENAI_OAUTH_USAGE_ORIGINATOR)));
    assert!(openai_headers.contains(&("user-agent", OPENAI_OAUTH_USAGE_USER_AGENT)));
    assert!(
        !openai_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("openai-beta")),
        "WHAM is a JSON GET, not the experimental Responses request"
    );
    assert!(UsageMeterEndpoint::KimiOauth.extra_headers().is_empty());
    let anthropic_headers = UsageMeterEndpoint::AnthropicOauth.extra_headers();
    assert_eq!(anthropic_headers[0], ("anthropic-beta", "oauth-2025-04-20"));
    assert_eq!(anthropic_headers[1].0, "user-agent");
    assert!(
        anthropic_headers[1].1.starts_with("claude-code/"),
        "the endpoint refuses non-Claude-Code user agents"
    );
    assert!(UsageMeterEndpoint::OpenAiOauth.min_poll_interval_ms() >= 60_000);
    assert!(UsageMeterEndpoint::AnthropicOauth.min_poll_interval_ms() >= 180_000);
    assert!(UsageMeterEndpoint::KimiOauth.min_poll_interval_ms() >= 60_000);
}

/// LAW (rfc3339_parser_is_exact_to_the_millisecond_and_total): the
/// dependency-free RFC 3339 parser lands on the exact Unix millisecond for
/// zulu, positive-offset, and long-fraction forms — and refuses malformed
/// input with `None` rather than inventing an instant.
#[test]
fn rfc3339_parser_is_exact_to_the_millisecond_and_total() {
    assert_eq!(parse_rfc3339_to_unix_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_rfc3339_to_unix_ms("2026-08-05T09:50:00.833669+00:00"),
        Some(1_785_923_400_833)
    );
    assert_eq!(
        parse_rfc3339_to_unix_ms("2026-08-05T15:20:00+05:30"),
        Some(1_785_923_400_000),
        "offset arithmetic lands on the same instant"
    );
    assert_eq!(
        parse_rfc3339_to_unix_ms("2026-01-06T13:33:02.717479433Z"),
        Some(1_767_706_382_717),
        "nanosecond fractions truncate to milliseconds"
    );
    for malformed in [
        "",
        "yesterday",
        "2026-08-05",
        "2026-08-05T09:50:00",
        "2026-13-05T09:50:00Z",
        "2026-08-05T25:50:00Z",
        "2026-08-05T09:50:00.Z",
        "2026-08-05T09:50:00+5:30",
        "1969-12-31T23:59:59Z",
    ] {
        assert_eq!(
            parse_rfc3339_to_unix_ms(malformed),
            None,
            "must refuse {malformed:?}"
        );
    }
}

/// LAW (pricing_estimates_known_families_and_refuses_unknown_models): the
/// bundled table prices by longest prefix over the normalized id (dated
/// releases price like their family, `models/` strips, `gpt-5.3-codex`
/// beats `gpt-5`), the arithmetic is exact per million tokens with
/// reasoning billed as output and cache reads at the cache rate — and an
/// unknown model yields `None`, never an invented rate.
#[test]
fn pricing_estimates_known_families_and_refuses_unknown_models() {
    assert_eq!(
        model_rate("claude-sonnet-4-5-20250929")
            .expect("sonnet family priced")
            .prefix,
        "claude-sonnet-4"
    );
    assert_eq!(
        model_rate("models/gemini-2.5-pro")
            .expect("gemini priced")
            .prefix,
        "gemini-2.5-pro"
    );
    assert_eq!(
        model_rate("gpt-5.3-codex")
            .expect("codex family priced")
            .prefix,
        "gpt-5.3-codex",
        "longest prefix beats the gpt-5 family row"
    );
    assert_eq!(model_rate("mystery-9"), None);
    assert_eq!(estimate_chunk_cost_usd("mystery-9", 1, 1, 1, 1), None);

    // claude-sonnet-4* rate: 3/15 with 0.3 cache reads. 1M in, 100k out,
    // 50k reasoning (billed as output), 200k cached.
    let cost = estimate_chunk_cost_usd("claude-sonnet-4-5", 1_000_000, 100_000, 50_000, 200_000)
        .expect("priced");
    let expected = 3.0 + 0.15 * 15.0 + 0.2 * 0.3;
    assert!(
        (cost - expected).abs() < 1e-9,
        "cost {cost} != expected {expected}"
    );

    // Zero tokens price to zero dollars, not None — the model is known.
    assert_eq!(
        estimate_chunk_cost_usd("claude-haiku-4-5", 0, 0, 0, 0),
        Some(0.0)
    );
}

/// CM1b — the compatibility pricing fold subtracts cache reads from
/// subset-style total input instead of charging them twice.
///
/// MUTATION CHECK (executed): restore `input*R + cached*C`; the OpenAI cost
/// becomes 1.35 instead of 0.35 and the Gemini cost becomes 0.327 instead of
/// 0.057.
#[test]
fn cm1b_subset_pricing_does_not_double_count_openai_or_gemini_reads() {
    let openai = estimate_chunk_cost_usd("gpt-5", 1_000_000, 0, 0, 800_000).expect("OpenAI price");
    assert!((openai - 0.35).abs() < 1e-12, "OpenAI cost: {openai}");
    let gemini = estimate_chunk_cost_usd("gemini-2.5-flash", 1_000_000, 0, 0, 900_000)
        .expect("Gemini price");
    assert!((gemini - 0.057).abs() < 1e-12, "Gemini cost: {gemini}");
}

/// Q4 law: current usage is priced by provider and model together. A custom
/// router serving a built-in-looking slug has no known price; it must not
/// inherit the built-in vendor's totals or cache economics.
///
/// MUTATION CHECK: replace any qualified lookup below with its model-only
/// compatibility lookup. The three `router-lab` assertions become `Some`.
#[test]
fn custom_provider_model_slug_collision_stays_unpriced() {
    let usage = NormalizedUsage {
        logical_input: 1_000_000,
        uncached_input: 200_000,
        cache_read_input: 800_000,
        billed_output: 100_000,
        cache_status: CacheStatAvailability::Present,
        cache_telemetry_input: 1_000_000,
        ..NormalizedUsage::default()
    };

    assert_eq!(
        estimate_chunk_cost_usd_for("router-lab", "gpt-5", 1_000_000, 100_000, 0, 800_000),
        None
    );
    assert_eq!(
        estimate_normalized_usage_cost_usd_for("router-lab", "gpt-5", &usage),
        None
    );
    assert_eq!(
        estimate_cache_input_costs_for("router-lab", "gpt-5", &usage),
        None
    );

    assert!(
        estimate_normalized_usage_cost_usd_for("openai", "gpt-5", &usage).is_some(),
        "the provider-qualified path must retain registered built-in pricing"
    );
    assert!(
        estimate_cache_input_costs_for("openai", "gpt-5", &usage).is_some(),
        "the provider-qualified path must retain registered cache pricing"
    );
}

/// CM1e — an Anthropic 1h creation is billed at the registry's 2x write
/// rate.
///
/// MUTATION CHECK (executed): set the 1h multiplier to 1.0; the with-cache
/// input cost falls from $6 to $3 and this law fails.
#[test]
fn cm1e_anthropic_one_hour_write_premium_is_priced() {
    let usage = NormalizedUsage {
        logical_input: 1_000_000,
        uncached_input: 1_000_000,
        cache_read_input: 0,
        cache_write_input: 1_000_000,
        cache_write_5m_input: 0,
        cache_write_1h_input: 1_000_000,
        billed_output: 10,
        reasoning_detail: 10,
        reasoning_accounting: ReasoningAccounting::SubsetOfOutput,
        cache_status: CacheStatAvailability::Present,
        cache_write_status: CacheStatAvailability::Present,
        cache_write_ttl_status: CacheStatAvailability::Present,
        cache_telemetry_input: 1_000_000,
        explicit_cache_storage_token_hours: None,
    };
    let cost = estimate_cache_input_costs("claude-sonnet-5", &usage).expect("priced");
    assert!((cost.input_with_cache_usd - 6.0).abs() < 1e-12);
    assert!((cost.input_without_cache_usd - 3.0).abs() < 1e-12);
}

#[test]
fn cm1_model_registry_prices_gpt56_kimi_deepseek_and_explicit_storage() {
    let normalized = |uncached: u64, read: u64| NormalizedUsage {
        logical_input: uncached + read,
        uncached_input: uncached,
        cache_read_input: read,
        billed_output: 0,
        reasoning_accounting: ReasoningAccounting::SubsetOfOutput,
        cache_status: CacheStatAvailability::Present,
        cache_telemetry_input: uncached + read,
        ..NormalizedUsage::default()
    };

    let kimi = estimate_cache_input_costs("kimi-k3", &normalized(10_000_000, 90_000_000))
        .expect("Kimi cache rate");
    assert!((kimi.input_with_cache_usd - 57.0).abs() < 1e-12);

    let deepseek =
        estimate_cache_input_costs("deepseek-v4-flash", &normalized(10_000_000, 90_000_000))
            .expect("DeepSeek cache rate");
    assert!((deepseek.input_with_cache_usd - 1.652).abs() < 1e-12);

    let terra = estimate_cache_input_costs(
        "gpt-5.6-terra",
        &NormalizedUsage {
            cache_write_input: 2_000_000,
            cache_write_status: CacheStatAvailability::Present,
            ..normalized(10_000_000, 90_000_000)
        },
    )
    .expect("GPT-5.6 write surcharge");
    assert!((terra.input_with_cache_usd - 39.0).abs() < 1e-12);

    let gemini = estimate_cache_input_costs(
        "gemini-2.5-flash",
        &NormalizedUsage {
            explicit_cache_storage_token_hours: Some(2_000_000.0),
            ..normalized(10_000_000, 90_000_000)
        },
    )
    .expect("Gemini explicit storage");
    assert!((gemini.input_with_cache_usd - 7.7).abs() < 1e-12);
    assert!((gemini.explicit_storage_usd - 2.0).abs() < 1e-12);
}

/// LAW (CM3b): cold-minus-warm estimates come from the qualified cache-price
/// registry. The 1M-token equivalent multipliers are 1.15/1.90 for
/// Anthropic 5m/1h, 1.15 for GPT-5.6, model-specific 0.5–0.9 for older
/// OpenAI, 0.9 for Gemini/Kimi K3, and 0.98 for DeepSeek V4 Flash.
///
/// MUTATION CHECK (executed): replace the registry delta with a global 0.9,
/// or ignore the TTL selector; the exact equivalent-token/cost rows fail.
#[test]
fn cm3b_registry_estimates_invalidated_prefix_rewarm_cost() {
    let cases = [
        (
            "anthropic",
            "claude-sonnet-5",
            CacheWriteTtl::FiveMinutes,
            1_150_000.0,
            3.45,
        ),
        (
            "anthropic-oauth",
            "claude-sonnet-5",
            CacheWriteTtl::OneHour,
            1_900_000.0,
            5.70,
        ),
        (
            "openai",
            "gpt-5.6-terra",
            CacheWriteTtl::Default,
            1_150_000.0,
            2.30,
        ),
        ("openai", "gpt-4o", CacheWriteTtl::Default, 500_000.0, 1.25),
        ("openai", "gpt-5", CacheWriteTtl::Default, 900_000.0, 1.125),
        (
            "gemini",
            "gemini-2.5-flash",
            CacheWriteTtl::Default,
            900_000.0,
            0.27,
        ),
        (
            "kimi-oauth",
            "kimi-k3",
            CacheWriteTtl::Default,
            900_000.0,
            2.70,
        ),
        (
            "deepseek",
            "deepseek-v4-flash",
            CacheWriteTtl::Default,
            980_000.0,
            0.1372,
        ),
    ];
    for (provider, model, ttl, equivalents, usd) in cases {
        let estimate = estimate_cache_rewarm_cost_usd(provider, model, 1_000_000, ttl)
            .unwrap_or_else(|| panic!("missing {provider}/{model}"));
        assert_eq!(estimate.stable_prefix_tokens, 1_000_000);
        assert!(
            (estimate.base_input_equivalent_tokens - equivalents).abs() < 1e-9,
            "{provider}/{model}: {estimate:?}"
        );
        assert!(
            (estimate.extra_input_cost_usd - usd).abs() < 1e-9,
            "{provider}/{model}: {estimate:?}"
        );
    }
    assert!(
        estimate_cache_rewarm_cost_usd(
            "openai-compatible",
            "gpt-5",
            1_000_000,
            CacheWriteTtl::Default
        )
        .is_none(),
        "a compatible endpoint cannot inherit OpenAI economics by model name"
    );
}

/// LAW (review-of-record RV4): the anthropic-oauth meter request's REQUIRED
/// header pair is pinned by VALUE — `anthropic-beta: oauth-2025-04-20` and a
/// `claude-code/` user-agent. The endpoint refuses requests without them,
/// but only a gated live test would notice; this pin makes the contract
/// CI-observable. Kimi rides Bearer alone; OpenAI also carries its account
/// id dynamically in the daemon plus the client identity headers below.
///
/// MUTATION CHECK: corrupt either header value in
/// `UsageMeterEndpoint::extra_headers`. Expected failure: the exact-value
/// asserts below. The OpenAI meter is pinned here too: dropping originator
/// or User-Agent makes the companion exact-value assertions fail.
#[test]
fn anthropic_meter_request_carries_the_required_header_values() {
    let headers = UsageMeterEndpoint::AnthropicOauth.extra_headers();
    assert_eq!(headers.len(), 2, "beta + user-agent, nothing else");
    assert!(
        headers.contains(&("anthropic-beta", "oauth-2025-04-20")),
        "the beta header value is load-bearing: {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|(name, value)| *name == "user-agent" && value.starts_with("claude-code/")),
        "the endpoint requires a claude-code user-agent: {headers:?}"
    );
    let openai = UsageMeterEndpoint::OpenAiOauth.extra_headers();
    assert_eq!(openai.len(), 3, "accept + originator + user-agent");
    assert_eq!(
        openai,
        &[
            ("accept", "application/json"),
            ("originator", OPENAI_OAUTH_USAGE_ORIGINATOR),
            ("user-agent", OPENAI_OAUTH_USAGE_USER_AGENT),
        ]
    );
    assert!(UsageMeterEndpoint::KimiOauth.extra_headers().is_empty());
}
