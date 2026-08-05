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
//! - long-context surcharges, batch discounts, and cache-WRITE premiums are
//!   deliberately ignored — this is an ESTIMATE, never a bill.
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

/// Estimates USD cost for one model's token chunk.
///
/// `reasoning` bills at the output rate (providers meter reasoning as output
/// tokens); `cached` bills at the cache-read rate when the table has one,
/// otherwise at the full input rate. Unknown model → `None`, never a guess.
pub fn estimate_chunk_cost_usd(
    model: &str,
    input: u64,
    output: u64,
    reasoning: u64,
    cached: u64,
) -> Option<f64> {
    let rate = model_rate(model)?;
    let per_token = |count: u64, per_mtok: f64| (count as f64) * per_mtok / 1_000_000.0;
    let cached_rate = rate.cached_input_per_mtok.unwrap_or(rate.input_per_mtok);
    Some(
        per_token(input, rate.input_per_mtok)
            + per_token(output.saturating_add(reasoning), rate.output_per_mtok)
            + per_token(cached, cached_rate),
    )
}
