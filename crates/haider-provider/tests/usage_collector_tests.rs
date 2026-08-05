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

use haider_provider::{
    ANTHROPIC_OAUTH_USAGE_URL, KIMI_OAUTH_USAGE_URL, MeterUnavailable, OPENAI_OAUTH_USAGE_URL,
    UsageMeterEndpoint, estimate_chunk_cost_usd, model_rate, normalize_utilization,
    parse_rfc3339_to_unix_ms,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/usage")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {name}"))
}

/// LAW (openai_wham_fixture_yields_primary_secondary_and_named_extra_windows):
/// the codex meter parse maps `rate_limit.primary_window`/`secondary_window`
/// to the `primary`/`secondary` windows (percent → 0–1 fraction, `reset_at`
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
    assert_eq!(reading.windows[0].window, "primary");
    assert!((reading.windows[0].utilization - 0.06).abs() < 1e-9);
    assert_eq!(reading.windows[0].resets_at_ms, Some(1_738_300_000_000));
    assert_eq!(reading.windows[0].label, None);
    assert_eq!(reading.windows[1].window, "secondary");
    assert!((reading.windows[1].utilization - 0.24).abs() < 1e-9);
    assert_eq!(reading.windows[1].resets_at_ms, Some(1_738_900_000_000));
    assert_eq!(reading.windows[2].window, "primary");
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
/// beta + Claude Code User-Agent headers while the others add none; and the
/// cache floors honor the brief (codex ≥ 60 s, anthropic ≥ 180 s).
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
    assert!(UsageMeterEndpoint::OpenAiOauth.extra_headers().is_empty());
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

/// LAW (review-of-record RV4): the anthropic-oauth meter request's REQUIRED
/// header pair is pinned by VALUE — `anthropic-beta: oauth-2025-04-20` and a
/// `claude-code/` user-agent. The endpoint refuses requests without them,
/// but only a gated live test would notice; this pin makes the contract
/// CI-observable. openai/kimi meters ride Bearer alone.
///
/// MUTATION CHECK: corrupt either header value in
/// `UsageMeterEndpoint::extra_headers`. Expected failure: the exact-value
/// asserts below.
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
    assert!(UsageMeterEndpoint::OpenAiOauth.extra_headers().is_empty());
    assert!(UsageMeterEndpoint::KimiOauth.extra_headers().is_empty());
}
