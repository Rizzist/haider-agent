//! Documented provider/model cacheability thresholds and cache observations.
//!
//! This table is deliberately incomplete. A missing row means that the
//! provider has not published a current minimum for that provider/model pair;
//! it must never be replaced with an observed value or an old implementation
//! detail. In particular, DeepSeek's current V4 guide publishes no numeric
//! minimum, so DeepSeek has no row here.

use haider_protocol::provider::{CacheStatAvailability, NormalizedUsage};

use crate::{
    ANTHROPIC_PROVIDER_NAME, BEDROCK_PROVIDER_NAME, GEMINI_PROVIDER_NAME, OPENAI_PROVIDER_NAME,
    VERTEX_PROVIDER_NAME,
};

const ANTHROPIC_CACHE_DOCUMENTATION: &str =
    "https://platform.claude.com/docs/en/build-with-claude/prompt-caching";
const BEDROCK_CACHE_DOCUMENTATION: &str =
    "https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html";
const OPENAI_CACHE_DOCUMENTATION: &str =
    "https://developers.openai.com/api/docs/guides/prompt-caching";
const GEMINI_CACHE_DOCUMENTATION: &str =
    "https://ai.google.dev/gemini-api/docs/generate-content/caching";

/// A provider/model family's documented minimum cacheable prompt length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheablePromptMinimumPolicy {
    /// Exact provider names for which the source documents this threshold.
    pub providers: &'static [&'static str],
    /// Canonical model family; recognized dated snapshots share the row.
    pub model_family: &'static str,
    pub minimum_tokens: u64,
    pub documentation_url: &'static str,
}

/// Current, source-backed cacheability thresholds.
///
/// Anthropic explicitly publishes one table for the Claude API and Google
/// Cloud. Bedrock publishes platform-specific values, so its rows remain
/// separate even when the model name is the same.
pub const CACHEABLE_PROMPT_MINIMUM_POLICIES: &[CacheablePromptMinimumPolicy] = &[
    // Claude API and Claude on Vertex.
    anthropic_minimum("claude-opus-5", 512),
    anthropic_minimum("claude-fable-5", 512),
    anthropic_minimum("claude-mythos-5", 512),
    anthropic_minimum("claude-mythos-preview", 2_048),
    anthropic_minimum("claude-opus-4-7", 2_048),
    anthropic_minimum("claude-opus-4-6", 4_096),
    anthropic_minimum("claude-opus-4-5", 4_096),
    anthropic_minimum("claude-opus-4-8", 1_024),
    anthropic_minimum("claude-sonnet-5", 1_024),
    anthropic_minimum("claude-sonnet-4-6", 1_024),
    anthropic_minimum("claude-sonnet-4-5", 1_024),
    anthropic_minimum("claude-opus-4-1", 1_024),
    anthropic_minimum("claude-opus-4", 1_024),
    anthropic_minimum("claude-sonnet-4", 1_024),
    anthropic_minimum("claude-haiku-4-5", 4_096),
    anthropic_minimum("claude-3-5-haiku", 2_048),
    // Bedrock Mantle model cards / Bedrock cache table. These values are
    // intentionally not inherited from the Claude API rows above.
    bedrock_minimum("anthropic.claude-fable-5", 1_024),
    bedrock_minimum("anthropic.claude-opus-5", 512),
    bedrock_minimum("anthropic.claude-mythos-5", 1_024),
    bedrock_minimum("anthropic.claude-mythos-preview", 4_096),
    bedrock_minimum("anthropic.claude-opus-4-8", 4_096),
    bedrock_minimum("anthropic.claude-opus-4-7", 4_096),
    bedrock_minimum("anthropic.claude-opus-4-6", 4_096),
    bedrock_minimum("anthropic.claude-opus-4-5", 4_096),
    bedrock_minimum("anthropic.claude-sonnet-5", 4_096),
    bedrock_minimum("anthropic.claude-sonnet-4-6", 1_024),
    bedrock_minimum("anthropic.claude-sonnet-4-5", 4_096),
    bedrock_minimum("anthropic.claude-haiku-4-5", 4_096),
    bedrock_minimum("anthropic.claude-opus-4", 1_024),
    bedrock_minimum("anthropic.claude-3-7-sonnet", 1_024),
    bedrock_minimum("anthropic.claude-3-5-sonnet", 1_024),
    // OpenAI documents a strict 1,024-token minimum for GPT-5.6 and later.
    openai_minimum("gpt-5.6", 1_024),
    openai_minimum("gpt-5.6-sol", 1_024),
    openai_minimum("gpt-5.6-terra", 1_024),
    openai_minimum("gpt-5.6-luna", 1_024),
    // Gemini explicit-context-cache table.
    gemini_minimum("gemini-3.7-flash", 4_096),
    gemini_minimum("gemini-3.6-flash", 4_096),
    gemini_minimum("gemini-3.5-flash", 4_096),
    gemini_minimum("gemini-3.1-pro-preview", 4_096),
    gemini_minimum("gemini-2.5-flash", 2_048),
    gemini_minimum("gemini-2.5-pro", 2_048),
];

const fn anthropic_minimum(
    model_family: &'static str,
    minimum_tokens: u64,
) -> CacheablePromptMinimumPolicy {
    CacheablePromptMinimumPolicy {
        providers: &[ANTHROPIC_PROVIDER_NAME, VERTEX_PROVIDER_NAME],
        model_family,
        minimum_tokens,
        documentation_url: ANTHROPIC_CACHE_DOCUMENTATION,
    }
}

const fn bedrock_minimum(
    model_family: &'static str,
    minimum_tokens: u64,
) -> CacheablePromptMinimumPolicy {
    CacheablePromptMinimumPolicy {
        providers: &[BEDROCK_PROVIDER_NAME],
        model_family,
        minimum_tokens,
        documentation_url: BEDROCK_CACHE_DOCUMENTATION,
    }
}

const fn openai_minimum(
    model_family: &'static str,
    minimum_tokens: u64,
) -> CacheablePromptMinimumPolicy {
    CacheablePromptMinimumPolicy {
        providers: &[OPENAI_PROVIDER_NAME],
        model_family,
        minimum_tokens,
        documentation_url: OPENAI_CACHE_DOCUMENTATION,
    }
}

const fn gemini_minimum(
    model_family: &'static str,
    minimum_tokens: u64,
) -> CacheablePromptMinimumPolicy {
    CacheablePromptMinimumPolicy {
        providers: &[GEMINI_PROVIDER_NAME],
        model_family,
        minimum_tokens,
        documentation_url: GEMINI_CACHE_DOCUMENTATION,
    }
}

/// Interpretation of provider cache-read telemetry for a cacheable prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheUsageAssessment {
    /// The provider reported that at least one input token came from cache.
    Hit,
    /// The prefix is shorter than this provider/model's documented minimum.
    BelowCacheableMinimum { minimum_tokens: u64 },
    /// The provider reported zero cache reads for an eligible prefix.
    Missed,
    /// No reliable distinction can be made from the published contract and
    /// the telemetry that arrived.
    Unavailable,
}

/// Finds the documented cacheability policy for an exact provider/model pair.
///
/// Provider names are exact on purpose: a generic endpoint, OAuth proxy, or
/// gateway does not inherit an upstream guarantee merely because its model
/// string resembles one of these families.
#[must_use]
pub fn cacheable_prompt_minimum_policy(
    provider: &str,
    model: &str,
) -> Option<&'static CacheablePromptMinimumPolicy> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    CACHEABLE_PROMPT_MINIMUM_POLICIES.iter().find(|policy| {
        policy.providers.contains(&provider) && model_family_matches(model, policy.model_family)
    })
}

/// Returns a provider/model's documented minimum cacheable prompt length.
#[must_use]
pub fn cacheable_prompt_minimum(provider: &str, model: &str) -> Option<u64> {
    cacheable_prompt_minimum_policy(provider, model).map(|policy| policy.minimum_tokens)
}

/// Distinguishes an expected below-minimum zero from an eligible cache miss.
///
/// Positive, present provider telemetry proves a hit even if the threshold is
/// unknown. A zero only proves a miss when the counter was present *and* the
/// stable prefix meets a documented minimum. Missing telemetry never becomes
/// a synthetic measured zero.
#[must_use]
pub fn assess_cache_usage(
    provider: &str,
    model: &str,
    stable_prefix_tokens: u64,
    usage: Option<&NormalizedUsage>,
) -> CacheUsageAssessment {
    if usage.is_some_and(|usage| {
        usage.cache_status == CacheStatAvailability::Present && usage.cache_read_input > 0
    }) {
        return CacheUsageAssessment::Hit;
    }

    let Some(minimum_tokens) = cacheable_prompt_minimum(provider, model) else {
        return CacheUsageAssessment::Unavailable;
    };
    if stable_prefix_tokens < minimum_tokens {
        return CacheUsageAssessment::BelowCacheableMinimum { minimum_tokens };
    }
    if usage.is_some_and(|usage| usage.cache_status == CacheStatAvailability::Present) {
        CacheUsageAssessment::Missed
    } else {
        CacheUsageAssessment::Unavailable
    }
}

fn model_family_matches(model: &str, family: &str) -> bool {
    if model == family {
        return true;
    }
    let Some(suffix) = model.strip_prefix(family) else {
        return false;
    };
    snapshot_suffix(suffix)
}

fn snapshot_suffix(suffix: &str) -> bool {
    if let Some(date) = suffix.strip_prefix('@') {
        return eight_digits(date);
    }
    let Some(version) = suffix.strip_prefix('-') else {
        return false;
    };
    let bytes = version.as_bytes();
    if bytes
        .get(..8)
        .is_some_and(|date| date.iter().all(u8::is_ascii_digit))
    {
        return version.len() == 8 || bedrock_version_suffix(&version[8..]);
    }
    if bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return version.len() == 10 || bedrock_version_suffix(&version[10..]);
    }
    bedrock_version_suffix(suffix)
}

fn eight_digits(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn bedrock_version_suffix(suffix: &str) -> bool {
    let Some(version) = suffix.strip_prefix("-v") else {
        return false;
    };
    match version.split_once(':') {
        Some((major, minor)) => {
            !major.is_empty()
                && major.bytes().all(|byte| byte.is_ascii_digit())
                && !minor.is_empty()
                && minor.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reported_cache_read(tokens: u64) -> NormalizedUsage {
        NormalizedUsage {
            cache_read_input: tokens,
            cache_status: CacheStatAvailability::Present,
            ..NormalizedUsage::default()
        }
    }

    #[test]
    fn documented_minimums_are_provider_and_model_specific() {
        assert_eq!(
            cacheable_prompt_minimum(ANTHROPIC_PROVIDER_NAME, "claude-opus-5-20260801"),
            Some(512)
        );
        assert_eq!(
            cacheable_prompt_minimum(VERTEX_PROVIDER_NAME, "claude-sonnet-4-5@20250929"),
            Some(1_024)
        );
        assert_eq!(
            cacheable_prompt_minimum(BEDROCK_PROVIDER_NAME, "anthropic.claude-fable-5"),
            Some(1_024),
            "Bedrock's documented minimum differs from the Claude API"
        );
        assert_eq!(
            cacheable_prompt_minimum(GEMINI_PROVIDER_NAME, "models/gemini-3.7-flash"),
            Some(4_096)
        );
        assert_eq!(
            cacheable_prompt_minimum(OPENAI_PROVIDER_NAME, "gpt-5.6-terra"),
            Some(1_024)
        );
    }

    /// MUTATION CHECK: broaden family matching to arbitrary suffixes. Expected
    /// failure: undocumented future variants inherit a stale threshold.
    #[test]
    fn undocumented_provider_or_model_minimum_stays_unknown() {
        for (provider, model) in [
            (crate::DEEPSEEK_PROVIDER_NAME, "deepseek-v4"),
            (crate::KIMI_OAUTH_PROVIDER_NAME, "kimi-k3"),
            (crate::OPENAI_OAUTH_PROVIDER_NAME, "gpt-5.6-sol"),
            (crate::HAIDER_CODE_PROVIDER_NAME, "gpt-5.6-sol"),
            (GEMINI_PROVIDER_NAME, "gemini-2.5-flash-lite"),
            (ANTHROPIC_PROVIDER_NAME, "claude-opus-5-1"),
        ] {
            assert_eq!(cacheable_prompt_minimum(provider, model), None);
        }
        assert_eq!(
            cacheable_prompt_minimum(OPENAI_PROVIDER_NAME, "gpt-5.6-é"),
            None
        );
    }

    /// MUTATION CHECK: change the GPT-5.6 threshold to 1,025. Expected
    /// failure: the exact documented boundary is labeled below-minimum.
    #[test]
    fn present_zero_at_minimum_is_a_miss_but_shorter_is_below_minimum() {
        let zero = reported_cache_read(0);
        assert_eq!(
            assess_cache_usage(OPENAI_PROVIDER_NAME, "gpt-5.6-sol", 1_023, Some(&zero)),
            CacheUsageAssessment::BelowCacheableMinimum {
                minimum_tokens: 1_024
            }
        );
        assert_eq!(
            assess_cache_usage(OPENAI_PROVIDER_NAME, "gpt-5.6-sol", 1_024, Some(&zero)),
            CacheUsageAssessment::Missed
        );
    }

    /// MUTATION CHECK: infer a miss from the numeric zero without consulting
    /// availability. Expected failure: absent telemetry becomes `Missed`.
    #[test]
    fn absent_telemetry_never_becomes_a_measured_zero() {
        let absent = NormalizedUsage::default();
        assert_eq!(
            assess_cache_usage(OPENAI_PROVIDER_NAME, "gpt-5.6", 1_024, Some(&absent)),
            CacheUsageAssessment::Unavailable
        );
    }

    /// MUTATION CHECK: assign DeepSeek the obsolete 64-token implementation
    /// detail. Expected failure: its reported zero becomes a claimed miss.
    #[test]
    fn zero_with_unknown_minimum_stays_unavailable() {
        let zero = reported_cache_read(0);
        assert_eq!(
            assess_cache_usage(
                crate::DEEPSEEK_PROVIDER_NAME,
                "deepseek-v4",
                10_000,
                Some(&zero)
            ),
            CacheUsageAssessment::Unavailable
        );
    }

    #[test]
    fn positive_provider_telemetry_proves_a_hit_without_a_known_minimum() {
        let hit = reported_cache_read(64);
        assert_eq!(
            assess_cache_usage(crate::DEEPSEEK_PROVIDER_NAME, "deepseek-v4", 64, Some(&hit)),
            CacheUsageAssessment::Hit
        );
    }
}
