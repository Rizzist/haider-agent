//! Bundled static model pricing table (U1) for LOCAL cost estimates.
//!
//! Source + date: snapshot taken 2026-08-05 from the providers' published
//! per-token API rates (Anthropic platform pricing docs; OpenAI API pricing
//! after the 2026-07-30 price cut; Google Gemini API pricing), corroborated
//! against public trackers (OpenRouter, BenchLM, pricepertoken) where the
//! vendor page was unreachable from the build environment. Rates are USD per
//! MILLION tokens. Notes:
//! - `claude-sonnet-5` is pinned at the standard 3/15 rate (an introductory
//!   2/10 rate runs through 2026-08-31);
//! - `gemini-3.5-flash` is pinned at the post-cut 0.75/4.50 rate;
//! - long-context surcharges and batch discounts are ignored; cache reads,
//!   cache writes, and explicit-storage rates are model/provider registry
//!   data and are applied only when normalized telemetry is sufficient;
//!
//! Matching is longest-prefix over the normalized model id (lowercased, a
//! leading `models/` stripped), so dated releases (`claude-opus-5-20260115`)
//! price like their family. Unknown models price to `None` — the estimator
//! never invents a rate.

/// USD per million tokens for one model family.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRate {
    /// Longest-prefix key over the normalized model id.
    pub prefix: &'static str,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Cache-READ rate; `None` bills cached tokens at the full input rate
    /// (the conservative direction).
    pub cached_input_per_mtok: Option<f64>,
}

/// Whether the legacy `Usage.input` counter includes cache reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheReadSemantics {
    /// OpenAI/Gemini/Kimi-style totals include the read subset.
    SubsetOfInput,
    /// Anthropic/DeepSeek-style legacy counters report reads separately.
    SeparateFromInput,
}

/// Cache-specific registry row. Longest-prefix matching is identical to the
/// base model table. Optional input/read overrides cover providers for which
/// CM1 has authoritative input-cache pricing but not a complete output-rate
/// row; the estimator never invents an output price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachePricingPolicy {
    pub prefix: &'static str,
    pub read_semantics: CacheReadSemantics,
    pub input_per_mtok: Option<f64>,
    pub cached_input_per_mtok: Option<f64>,
    pub default_write_multiplier: f64,
    pub write_5m_multiplier: f64,
    pub write_1h_multiplier: f64,
    pub storage_per_mtok_hour: Option<f64>,
    /// A write counter is required to price this family honestly.
    pub requires_write_telemetry: bool,
}

/// Provider/model-versioned cache policy registry. No universal cache-read
/// or cache-write constant is used by the fold.
pub const CACHE_PRICING_POLICIES: &[CachePricingPolicy] = &[
    CachePricingPolicy {
        prefix: "claude-",
        read_semantics: CacheReadSemantics::SeparateFromInput,
        input_per_mtok: None,
        cached_input_per_mtok: None,
        default_write_multiplier: 1.25,
        write_5m_multiplier: 1.25,
        write_1h_multiplier: 2.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: true,
    },
    CachePricingPolicy {
        prefix: "gpt-5.6-sol",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: None,
        default_write_multiplier: 1.25,
        write_5m_multiplier: 1.25,
        write_1h_multiplier: 1.25,
        storage_per_mtok_hour: None,
        requires_write_telemetry: true,
    },
    CachePricingPolicy {
        prefix: "gpt-5.6-terra",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: None,
        default_write_multiplier: 1.25,
        write_5m_multiplier: 1.25,
        write_1h_multiplier: 1.25,
        storage_per_mtok_hour: None,
        requires_write_telemetry: true,
    },
    CachePricingPolicy {
        prefix: "gpt-5.6-luna",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: None,
        default_write_multiplier: 1.25,
        write_5m_multiplier: 1.25,
        write_1h_multiplier: 1.25,
        storage_per_mtok_hour: None,
        requires_write_telemetry: true,
    },
    CachePricingPolicy {
        prefix: "gpt-",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: None,
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "gemini-2.5-pro",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: Some(0.125),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: Some(4.5),
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "gemini-2.5-flash",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: Some(0.03),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: Some(1.0),
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "gemini-3.1-pro",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: Some(0.2),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "gemini-3.5-flash",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: None,
        cached_input_per_mtok: Some(0.075),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "kimi-k3",
        read_semantics: CacheReadSemantics::SubsetOfInput,
        input_per_mtok: Some(3.0),
        cached_input_per_mtok: Some(0.3),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: false,
    },
    CachePricingPolicy {
        prefix: "deepseek-v4-flash",
        read_semantics: CacheReadSemantics::SeparateFromInput,
        input_per_mtok: Some(0.14),
        cached_input_per_mtok: Some(0.0028),
        default_write_multiplier: 1.0,
        write_5m_multiplier: 1.0,
        write_1h_multiplier: 1.0,
        storage_per_mtok_hour: None,
        requires_write_telemetry: false,
    },
];

/// The bundled table (snapshot 2026-08-05; see module docs for sources).
pub const MODEL_RATES: &[ModelRate] = &[
    // --- Anthropic ---
    ModelRate {
        prefix: "claude-fable-5",
        input_per_mtok: 10.0,
        output_per_mtok: 50.0,
        cached_input_per_mtok: Some(1.0),
    },
    ModelRate {
        prefix: "claude-opus-5",
        input_per_mtok: 5.0,
        output_per_mtok: 25.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "claude-opus-4",
        input_per_mtok: 15.0,
        output_per_mtok: 75.0,
        cached_input_per_mtok: Some(1.5),
    },
    // Opus 4.5 onward repriced to 5/25; plain `claude-opus-4` above keeps
    // the legacy 15/75 for opus-4 / opus-4-1 (dated ids match the family).
    ModelRate {
        prefix: "claude-opus-4-5",
        input_per_mtok: 5.0,
        output_per_mtok: 25.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "claude-opus-4-6",
        input_per_mtok: 5.0,
        output_per_mtok: 25.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "claude-opus-4-7",
        input_per_mtok: 5.0,
        output_per_mtok: 25.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "claude-opus-4-8",
        input_per_mtok: 5.0,
        output_per_mtok: 25.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "claude-sonnet-5",
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
        cached_input_per_mtok: Some(0.3),
    },
    ModelRate {
        prefix: "claude-sonnet-4",
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
        cached_input_per_mtok: Some(0.3),
    },
    ModelRate {
        prefix: "claude-haiku-4",
        input_per_mtok: 1.0,
        output_per_mtok: 5.0,
        cached_input_per_mtok: Some(0.1),
    },
    // --- OpenAI (post 2026-07-30 cut) ---
    ModelRate {
        prefix: "gpt-5.6-sol",
        input_per_mtok: 5.0,
        output_per_mtok: 30.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "gpt-5.6-terra",
        input_per_mtok: 2.0,
        output_per_mtok: 12.0,
        cached_input_per_mtok: Some(0.2),
    },
    ModelRate {
        prefix: "gpt-5.6-luna",
        input_per_mtok: 0.2,
        output_per_mtok: 1.2,
        cached_input_per_mtok: Some(0.02),
    },
    ModelRate {
        prefix: "gpt-5.5",
        input_per_mtok: 5.0,
        output_per_mtok: 30.0,
        cached_input_per_mtok: Some(0.5),
    },
    ModelRate {
        prefix: "gpt-5.3-codex",
        input_per_mtok: 1.75,
        output_per_mtok: 14.0,
        cached_input_per_mtok: Some(0.175),
    },
    ModelRate {
        prefix: "gpt-5.2",
        input_per_mtok: 1.25,
        output_per_mtok: 10.0,
        cached_input_per_mtok: Some(0.125),
    },
    ModelRate {
        prefix: "gpt-5.1-codex-mini",
        input_per_mtok: 0.25,
        output_per_mtok: 2.0,
        cached_input_per_mtok: Some(0.025),
    },
    ModelRate {
        prefix: "gpt-5.1",
        input_per_mtok: 1.25,
        output_per_mtok: 10.0,
        cached_input_per_mtok: Some(0.125),
    },
    ModelRate {
        prefix: "gpt-5",
        input_per_mtok: 1.25,
        output_per_mtok: 10.0,
        cached_input_per_mtok: Some(0.125),
    },
    ModelRate {
        prefix: "gpt-4o",
        input_per_mtok: 2.5,
        output_per_mtok: 10.0,
        cached_input_per_mtok: Some(1.25),
    },
    ModelRate {
        prefix: "gpt-4.1",
        input_per_mtok: 2.0,
        output_per_mtok: 8.0,
        cached_input_per_mtok: Some(0.5),
    },
    // --- Google Gemini ---
    ModelRate {
        prefix: "gemini-3.1-pro",
        input_per_mtok: 2.0,
        output_per_mtok: 12.0,
        cached_input_per_mtok: None,
    },
    ModelRate {
        prefix: "gemini-3.5-flash",
        input_per_mtok: 0.75,
        output_per_mtok: 4.5,
        cached_input_per_mtok: None,
    },
    ModelRate {
        prefix: "gemini-2.5-pro",
        input_per_mtok: 1.25,
        output_per_mtok: 10.0,
        cached_input_per_mtok: None,
    },
    ModelRate {
        prefix: "gemini-2.5-flash",
        input_per_mtok: 0.3,
        output_per_mtok: 2.5,
        cached_input_per_mtok: None,
    },
];

/// Longest-prefix rate lookup over the normalized model id.
pub fn model_rate(model: &str) -> Option<&'static ModelRate> {
    let normalized = model.trim().to_ascii_lowercase();
    let normalized = normalized.strip_prefix("models/").unwrap_or(&normalized);
    MODEL_RATES
        .iter()
        .filter(|rate| normalized.starts_with(rate.prefix))
        .max_by_key(|rate| rate.prefix.len())
}

/// Longest-prefix cache policy lookup.
pub fn cache_pricing_policy(model: &str) -> Option<&'static CachePricingPolicy> {
    let normalized = model.trim().to_ascii_lowercase();
    let normalized = normalized.strip_prefix("models/").unwrap_or(&normalized);
    CACHE_PRICING_POLICIES
        .iter()
        .filter(|policy| normalized.starts_with(policy.prefix))
        .max_by_key(|policy| policy.prefix.len())
}

/// Estimates USD cost for one model's token chunk.
///
/// This compatibility API retains the legacy counter arguments. Its fold is
/// nevertheless semantics-aware: subset-style providers subtract `cached`
/// from `input` before billing normal input, while separate-style providers
/// bill the counters independently. Unknown or malformed input → `None`.
pub fn estimate_chunk_cost_usd(
    model: &str,
    input: u64,
    output: u64,
    reasoning: u64,
    cached: u64,
) -> Option<f64> {
    let rate = model_rate(model)?;
    let per_token = |count: u64, per_mtok: f64| (count as f64) * per_mtok / 1_000_000.0;
    let policy = cache_pricing_policy(model)?;
    let cached_rate = policy
        .cached_input_per_mtok
        .or(rate.cached_input_per_mtok)
        .unwrap_or(rate.input_per_mtok);
    let normal_input = match policy.read_semantics {
        CacheReadSemantics::SubsetOfInput => input.checked_sub(cached)?,
        CacheReadSemantics::SeparateFromInput => input,
    };
    Some(
        per_token(normal_input, rate.input_per_mtok)
            + per_token(output.saturating_add(reasoning), rate.output_per_mtok)
            + per_token(cached, cached_rate),
    )
}

/// Prices normalized cache input using the model/provider registry.
/// Returns `None` rather than claiming savings when the read split, required
/// write counter, model input rate, or explicit-storage rate is unknown.
pub fn estimate_cache_input_costs(
    model: &str,
    usage: &haider_protocol::provider::NormalizedUsage,
) -> Option<haider_protocol::provider::CacheCostEstimate> {
    use haider_protocol::provider::{CacheCostEstimate, CacheStatAvailability};

    if usage.cache_status != CacheStatAvailability::Present
        || usage.cache_telemetry_input != usage.logical_input
    {
        return None;
    }
    let policy = cache_pricing_policy(model)?;
    let base = model_rate(model);
    let input_rate = policy
        .input_per_mtok
        .or_else(|| base.map(|rate| rate.input_per_mtok))?;
    let cached_rate = policy
        .cached_input_per_mtok
        .or_else(|| base.and_then(|rate| rate.cached_input_per_mtok))
        .unwrap_or(input_rate);
    if policy.requires_write_telemetry && usage.cache_write_status != CacheStatAvailability::Present
    {
        return None;
    }
    let fresh = usage.uncached_input.checked_sub(usage.cache_write_input)?;
    let per_token = |count: u64, per_mtok: f64| (count as f64) * per_mtok / 1_000_000.0;
    let write_cost = if usage.cache_write_ttl_status == CacheStatAvailability::Present {
        let split = usage
            .cache_write_5m_input
            .checked_add(usage.cache_write_1h_input)?;
        let remaining = usage.cache_write_input.checked_sub(split)?;
        per_token(
            usage.cache_write_5m_input,
            input_rate * policy.write_5m_multiplier,
        ) + per_token(
            usage.cache_write_1h_input,
            input_rate * policy.write_1h_multiplier,
        ) + per_token(remaining, input_rate * policy.default_write_multiplier)
    } else {
        per_token(
            usage.cache_write_input,
            input_rate * policy.default_write_multiplier,
        )
    };
    let explicit_storage_usd = match usage.explicit_cache_storage_token_hours {
        Some(token_hours) => token_hours * policy.storage_per_mtok_hour? / 1_000_000.0,
        None => 0.0,
    };
    let input_with_cache_usd = per_token(fresh, input_rate)
        + write_cost
        + per_token(usage.cache_read_input, cached_rate)
        + explicit_storage_usd;
    let input_without_cache_usd = per_token(
        usage.uncached_input.saturating_add(usage.cache_read_input),
        input_rate,
    );
    Some(CacheCostEstimate {
        input_with_cache_usd,
        input_without_cache_usd,
        estimated_savings_usd: input_without_cache_usd - input_with_cache_usd,
        explicit_storage_usd,
    })
}

/// Total normalized request estimate. When cache telemetry is unavailable,
/// logical input is conservatively billed at the normal rate; reasoning is
/// added only when the normalized accounting says it is additional to billed
/// output.
pub fn estimate_normalized_usage_cost_usd(
    model: &str,
    usage: &haider_protocol::provider::NormalizedUsage,
) -> Option<f64> {
    use haider_protocol::provider::ReasoningAccounting;

    let base = model_rate(model)?;
    let input_cost = estimate_cache_input_costs(model, usage)
        .map(|cost| cost.input_with_cache_usd)
        .unwrap_or_else(|| (usage.logical_input as f64) * base.input_per_mtok / 1_000_000.0);
    let billed_output = match usage.reasoning_accounting {
        ReasoningAccounting::AdditionalToOutput => {
            usage.billed_output.saturating_add(usage.reasoning_detail)
        }
        ReasoningAccounting::SubsetOfOutput | ReasoningAccounting::Unavailable => {
            usage.billed_output
        }
    };
    Some(input_cost + (billed_output as f64) * base.output_per_mtok / 1_000_000.0)
}
