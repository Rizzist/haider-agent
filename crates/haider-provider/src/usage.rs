//! Per-provider usage meter collectors (U1).
//!
//! Three OAuth providers expose a server-side meter; each parser here is a
//! pure function over recorded response bytes so fixtures replay without a
//! network. Response shapes were research-verified against real payloads:
//!
//! - codex/openai-oauth: `GET https://chatgpt.com/backend-api/wham/usage`
//!   (Bearer). Shape pinned from the codex ecosystem's own decoders —
//!   steipete/CodexBar `CodexOAuthUsageFetcher.swift` (`plan_type`,
//!   `rate_limit.primary_window/secondary_window` with
//!   `used_percent`/`reset_at`/`limit_window_seconds`,
//!   `additional_rate_limits[]` with `limit_name`) and
//!   luisleineweber/usagebar `docs/providers/codex.md` (sample payload).
//! - anthropic-oauth: `GET https://api.anthropic.com/api/oauth/usage`
//!   (Bearer + `anthropic-beta: oauth-2025-04-20` + mandatory `User-Agent:
//!   claude-code/<ver>` — the endpoint 403s without a Claude Code UA).
//!   Shape captured LIVE from a real Claude Max account on 2026-08-05
//!   (fixture `tests/fixtures/usage/anthropic_oauth_usage_live.json`):
//!   named buckets `five_hour`/`seven_day`/`seven_day_opus`/
//!   `seven_day_sonnet` (nullable) + `extra_usage`, RFC 3339 `resets_at`.
//! - kimi-oauth: `GET https://api.kimi.com/coding/v1/usages` (Bearer).
//!   Shape pinned from steipete/CodexBar `docs/kimi.md` and
//!   luisleineweber/usagebar `docs/providers/kimi.md`: quota counters are
//!   STRINGS (`"limit": "2048"`), windows carry
//!   `{duration, timeUnit: "TIME_UNIT_MINUTE"}`, ISO 8601 `resetTime`.
//!
//! DEFENSIVE NORMALIZATION: public sources disagree on the anthropic
//! `utilization` scale (0–1 fraction vs 0–100 percent — the live capture
//! says percent: `"utilization": 60.0` alongside `limits[].percent: 60`).
//! [`normalize_utilization`] therefore accepts BOTH scales and clamps; the
//! wire always carries the 0–1 fraction.
//!
//! API-key providers (openai/anthropic/gemini keys, custom
//! openai-compatible, OpenCode Zen/Go) have NO server meter — the daemon
//! reports honest local accounting for them. OpenCode Zen serves an
//! unauthenticated model inventory at `{base}/models` but `GET
//! /zen/v1/balance|usage|credits` all 404 (verified 2026-08-05); upstream
//! feature request anomalyco/opencode#10448 tracks a future balance
//! endpoint — when it ships, add an `OpenCodeZen` variant to
//! [`UsageMeterEndpoint`] and a parser law beside the existing three.
//!
//! Failure discipline: every failure path is a typed
//! [`MeterUnavailable`] with a bounded reason — never a panic, never a
//! fabricated reading, and never token material in the reason.

use haider_protocol::usage::UsageWindowV1;
use serde::Deserialize;

/// One successfully parsed meter reading.
#[derive(Debug, Clone, PartialEq)]
pub struct MeterReading {
    /// Normalized windows (utilization always 0.0–1.0).
    pub windows: Vec<UsageWindowV1>,
    /// Subscription plan when the meter reports one (codex `plan_type`).
    pub plan: Option<String>,
}

/// Typed unavailability: the honest terminal state of a failed reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterUnavailable {
    /// Bounded machine-readable `snake_case` reason (e.g. `http_status_429`,
    /// `malformed_response`). Never carries response or token bytes.
    pub reason: String,
}

impl MeterUnavailable {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The three OAuth meter endpoints served in this release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMeterEndpoint {
    /// codex / openai-oauth subscription meter.
    OpenAiOauth,
    /// Claude subscription meter.
    AnthropicOauth,
    /// Kimi coding-plan meter.
    KimiOauth,
}

/// `User-Agent` REQUIRED by the anthropic-oauth usage endpoint; requests
/// without a Claude Code UA are refused upstream.
pub const ANTHROPIC_OAUTH_USAGE_USER_AGENT: &str = "claude-code/2.0.19";
/// codex/openai-oauth usage meter URL.
pub const OPENAI_OAUTH_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// anthropic-oauth usage meter URL.
pub const ANTHROPIC_OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// kimi-oauth usage meter URL.
pub const KIMI_OAUTH_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

impl UsageMeterEndpoint {
    pub fn url(self) -> &'static str {
        match self {
            Self::OpenAiOauth => OPENAI_OAUTH_USAGE_URL,
            Self::AnthropicOauth => ANTHROPIC_OAUTH_USAGE_URL,
            Self::KimiOauth => KIMI_OAUTH_USAGE_URL,
        }
    }

    /// Extra request headers beyond `Authorization: Bearer <token>`.
    pub fn extra_headers(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::OpenAiOauth | Self::KimiOauth => &[],
            Self::AnthropicOauth => &[
                (
                    crate::ANTHROPIC_OAUTH_BETA_HEADER,
                    crate::ANTHROPIC_OAUTH_BETA_VALUE,
                ),
                ("user-agent", ANTHROPIC_OAUTH_USAGE_USER_AGENT),
            ],
        }
    }

    /// Minimum interval between live polls of this meter. Readings are
    /// cached; the daemon never contacts the endpoint more often than this.
    pub fn min_poll_interval_ms(self) -> u64 {
        match self {
            // Poll ≥ 60 s (the meter also backs codex's own status surface).
            Self::OpenAiOauth => 60_000,
            // Poll ≥ 180 s — the endpoint aggressively 429s tight pollers
            // (anthropics/claude-code#31637).
            Self::AnthropicOauth => 180_000,
            // Kimi documents no poll bound; stay comfortably conservative.
            Self::KimiOauth => 300_000,
        }
    }

    /// Parses one recorded response into a normalized reading.
    pub fn parse(self, status: u16, body: &[u8]) -> Result<MeterReading, MeterUnavailable> {
        if !(200..300).contains(&status) {
            return Err(MeterUnavailable::new(format!("http_status_{status}")));
        }
        match self {
            Self::OpenAiOauth => parse_openai_wham_usage(body),
            Self::AnthropicOauth => parse_anthropic_oauth_usage(body),
            Self::KimiOauth => parse_kimi_usages(body),
        }
    }
}

/// Normalizes a provider-reported utilization onto the 0–1 fraction scale.
///
/// Sources disagree on the scale (0–1 fraction vs 0–100 percent), so this is
/// deliberately defensive, monotone, and total:
/// - NaN → 0.0 (a reading we cannot order is not a reading);
/// - negative (including −∞) → 0.0;
/// - `0.0 ..= 1.0` → taken as an already-normalized fraction (1.0 = 100%);
/// - `1.0 < v <= 100.0` → taken as a percentage, divided by 100;
/// - above 100 (including +∞) → clamped to 1.0.
///
/// The one ambiguous point is exactly 1.0 (fraction 100% vs percent 1%): it
/// reads as the FRACTION, which can only over-report usage — the meter may
/// say "full" early but never "empty" when nearly exhausted.
pub fn normalize_utilization(raw: f64) -> f64 {
    if raw.is_nan() || raw <= 0.0 {
        return 0.0;
    }
    if raw <= 1.0 {
        return raw;
    }
    if raw <= 100.0 {
        return raw / 100.0;
    }
    1.0
}

// --- codex / openai-oauth -------------------------------------------------

#[derive(Debug, Deserialize)]
struct WhamUsage {
    plan_type: Option<String>,
    rate_limit: Option<WhamRateLimit>,
    #[serde(default)]
    additional_rate_limits: Vec<WhamAdditionalRateLimit>,
}

#[derive(Debug, Deserialize)]
struct WhamRateLimit {
    primary_window: Option<WhamWindow>,
    secondary_window: Option<WhamWindow>,
}

#[derive(Debug, Deserialize)]
struct WhamWindow {
    used_percent: Option<f64>,
    reset_at: Option<u64>,
    #[expect(dead_code, reason = "documented shape; window naming ignores it")]
    limit_window_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WhamAdditionalRateLimit {
    limit_name: Option<String>,
    rate_limit: Option<WhamRateLimit>,
}

fn wham_window(window: &WhamWindow, name: &str, label: Option<&str>) -> Option<UsageWindowV1> {
    let used_percent = window.used_percent?;
    Some(UsageWindowV1 {
        window: name.to_owned(),
        // `used_percent` is percent BY NAME — always divide, then clamp
        // through the shared normalizer for the defensive bounds.
        utilization: normalize_utilization(used_percent.clamp(0.0, 100.0) / 100.0),
        resets_at_ms: window.reset_at.map(|seconds| seconds.saturating_mul(1000)),
        label: label.map(str::to_owned),
    })
}

/// Parses `GET https://chatgpt.com/backend-api/wham/usage` bytes: primary
/// (5 h) + secondary (weekly) windows, named per-model
/// `additional_rate_limits`, and the subscription `plan_type`.
pub fn parse_openai_wham_usage(body: &[u8]) -> Result<MeterReading, MeterUnavailable> {
    let usage: WhamUsage = serde_json::from_slice(body)
        .map_err(|_| MeterUnavailable::new("malformed_response"))?;
    let mut windows = Vec::new();
    if let Some(rate_limit) = &usage.rate_limit {
        if let Some(window) = rate_limit
            .primary_window
            .as_ref()
            .and_then(|window| wham_window(window, "primary", None))
        {
            windows.push(window);
        }
        if let Some(window) = rate_limit
            .secondary_window
            .as_ref()
            .and_then(|window| wham_window(window, "secondary", None))
        {
            windows.push(window);
        }
    }
    for additional in &usage.additional_rate_limits {
        let Some(rate_limit) = &additional.rate_limit else {
            continue;
        };
        let label = additional.limit_name.as_deref();
        if let Some(window) = rate_limit
            .primary_window
            .as_ref()
            .and_then(|window| wham_window(window, "primary", label))
        {
            windows.push(window);
        }
        if let Some(window) = rate_limit
            .secondary_window
            .as_ref()
            .and_then(|window| wham_window(window, "secondary", label))
        {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err(MeterUnavailable::new("no_windows_reported"));
    }
    Ok(MeterReading {
        windows,
        plan: usage.plan_type,
    })
}

// --- anthropic-oauth ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AnthropicOauthUsage {
    five_hour: Option<AnthropicBucket>,
    seven_day: Option<AnthropicBucket>,
    seven_day_opus: Option<AnthropicBucket>,
    seven_day_sonnet: Option<AnthropicBucket>,
    extra_usage: Option<AnthropicExtraUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicBucket {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicExtraUsage {
    #[serde(default)]
    is_enabled: bool,
    utilization: Option<f64>,
}

fn anthropic_window(bucket: &AnthropicBucket, name: &str) -> Option<UsageWindowV1> {
    let utilization = bucket.utilization?;
    Some(UsageWindowV1 {
        window: name.to_owned(),
        utilization: normalize_utilization(utilization),
        resets_at_ms: bucket
            .resets_at
            .as_deref()
            .and_then(parse_rfc3339_to_unix_ms),
        label: None,
    })
}

/// Parses `GET https://api.anthropic.com/api/oauth/usage` bytes: the named
/// buckets (`five_hour`/`seven_day`/`seven_day_opus`/`seven_day_sonnet`,
/// each nullable) plus `extra_usage` when enabled. Utilization runs through
/// the defensive normalizer because sources disagree on its scale.
pub fn parse_anthropic_oauth_usage(body: &[u8]) -> Result<MeterReading, MeterUnavailable> {
    let usage: AnthropicOauthUsage = serde_json::from_slice(body)
        .map_err(|_| MeterUnavailable::new("malformed_response"))?;
    let mut windows = Vec::new();
    for (bucket, name) in [
        (&usage.five_hour, "five_hour"),
        (&usage.seven_day, "seven_day"),
        (&usage.seven_day_opus, "seven_day_opus"),
        (&usage.seven_day_sonnet, "seven_day_sonnet"),
    ] {
        if let Some(window) = bucket.as_ref().and_then(|bucket| anthropic_window(bucket, name)) {
            windows.push(window);
        }
    }
    if let Some(extra) = &usage.extra_usage
        && extra.is_enabled
        && let Some(utilization) = extra.utilization
    {
        windows.push(UsageWindowV1 {
            window: "extra_usage".to_owned(),
            utilization: normalize_utilization(utilization),
            resets_at_ms: None,
            label: None,
        });
    }
    if windows.is_empty() {
        return Err(MeterUnavailable::new("no_windows_reported"));
    }
    Ok(MeterReading {
        windows,
        plan: None,
    })
}

// --- kimi-oauth -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct KimiUsages {
    usage: Option<KimiQuota>,
    #[serde(default)]
    limits: Vec<KimiLimit>,
}

/// Kimi quota counters arrive as JSON STRINGS (`"limit": "2048"`).
#[derive(Debug, Deserialize)]
struct KimiQuota {
    limit: Option<String>,
    used: Option<String>,
    remaining: Option<String>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KimiLimit {
    window: Option<KimiWindowSpec>,
    detail: Option<KimiQuota>,
}

#[derive(Debug, Deserialize)]
struct KimiWindowSpec {
    duration: Option<u64>,
    #[serde(rename = "timeUnit")]
    time_unit: Option<String>,
}

fn kimi_utilization(quota: &KimiQuota) -> Option<f64> {
    let limit: f64 = quota.limit.as_deref()?.trim().parse().ok()?;
    if !(limit.is_finite() && limit > 0.0) {
        return None;
    }
    let used: f64 = match quota.used.as_deref() {
        Some(used) => used.trim().parse().ok()?,
        None => {
            let remaining: f64 = quota.remaining.as_deref()?.trim().parse().ok()?;
            limit - remaining
        }
    };
    Some(normalize_utilization((used / limit).clamp(0.0, 1.0)))
}

fn kimi_window_name(spec: Option<&KimiWindowSpec>) -> String {
    let Some(spec) = spec else {
        return "window".to_owned();
    };
    let Some(duration) = spec.duration else {
        return "window".to_owned();
    };
    let unit = match spec.time_unit.as_deref() {
        Some("TIME_UNIT_MINUTE") => "m",
        Some("TIME_UNIT_HOUR") => "h",
        Some("TIME_UNIT_DAY") => "d",
        _ => "u",
    };
    format!("window_{duration}{unit}")
}

/// Parses `GET https://api.kimi.com/coding/v1/usages` bytes: the plan-level
/// `usage` quota (string counters) plus each `limits[]` rolling window.
pub fn parse_kimi_usages(body: &[u8]) -> Result<MeterReading, MeterUnavailable> {
    let usages: KimiUsages = serde_json::from_slice(body)
        .map_err(|_| MeterUnavailable::new("malformed_response"))?;
    let mut windows = Vec::new();
    if let Some(quota) = &usages.usage
        && let Some(utilization) = kimi_utilization(quota)
    {
        windows.push(UsageWindowV1 {
            window: "quota".to_owned(),
            utilization,
            resets_at_ms: quota
                .reset_time
                .as_deref()
                .and_then(parse_rfc3339_to_unix_ms),
            label: None,
        });
    }
    for limit in &usages.limits {
        let Some(detail) = &limit.detail else {
            continue;
        };
        let Some(utilization) = kimi_utilization(detail) else {
            continue;
        };
        windows.push(UsageWindowV1 {
            window: kimi_window_name(limit.window.as_ref()),
            utilization,
            resets_at_ms: detail
                .reset_time
                .as_deref()
                .and_then(parse_rfc3339_to_unix_ms),
            label: None,
        });
    }
    if windows.is_empty() {
        return Err(MeterUnavailable::new("no_windows_reported"));
    }
    Ok(MeterReading {
        windows,
        plan: None,
    })
}

// --- RFC 3339 -------------------------------------------------------------

/// Parses an RFC 3339 timestamp (`2026-08-05T09:50:00.833669+00:00`,
/// fractional seconds optional, `Z` or `±HH:MM` offset) to Unix ms.
///
/// Hand-rolled on purpose: the workspace deliberately carries no calendar
/// dependency, and the meters only need millisecond fidelity. Returns `None`
/// on anything malformed — a reset time we cannot read is absent, never
/// invented. Pre-1970 instants are out of scope and read as `None`.
pub fn parse_rfc3339_to_unix_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    fn digits(bytes: &[u8]) -> Option<u64> {
        if bytes.is_empty() {
            return None;
        }
        let mut out: u64 = 0;
        for &byte in bytes {
            if !byte.is_ascii_digit() {
                return None;
            }
            out = out.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        }
        Some(out)
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut index = 19;
    let mut millis: u64 = 0;
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let fraction = &bytes[fraction_start..index];
        if fraction.is_empty() {
            return None;
        }
        // Millisecond fidelity: take up to three digits, zero-padded.
        let mut padded = [b'0'; 3];
        for (slot, &digit) in padded.iter_mut().zip(fraction.iter()) {
            *slot = digit;
        }
        millis = digits(&padded)?;
    }
    let offset_minutes: i64 = match bytes.get(index) {
        Some(b'Z' | b'z') if index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            let rest = &bytes[index + 1..];
            if rest.len() != 5 || rest[2] != b':' {
                return None;
            }
            let offset_hours = digits(&rest[0..2])?;
            let offset_mins = digits(&rest[3..5])?;
            if offset_hours > 23 || offset_mins > 59 {
                return None;
            }
            let total = i64::try_from(offset_hours * 60 + offset_mins).ok()?;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    // Days since the Unix epoch (Howard Hinnant's days-from-civil).
    let year = i64::try_from(year).ok()?;
    let month = i64::try_from(month).ok()?;
    let day = i64::try_from(day).ok()?;
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let month_shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400
        + i64::try_from(hour).ok()? * 3_600
        + i64::try_from(minute).ok()? * 60
        + i64::try_from(second).ok()?
        - offset_minutes * 60;
    let unix_ms = seconds.checked_mul(1_000)?.checked_add(
        i64::try_from(millis).ok()?,
    )?;
    u64::try_from(unix_ms).ok()
}
