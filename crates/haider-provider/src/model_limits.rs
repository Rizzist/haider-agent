//! Pinned per-model token limits: the static source for context windows and
//! output ceilings used when a provider catalog is unavailable or omits them.
//! Catalog declarations always win at daemon projection time
//! (`output_budget::model_output_limit`). Adapter capability tables
//! (`anthropic.rs`, `openai.rs`, `gemini.rs`) read their context windows from
//! here, so a row change moves both the daemon projection and the adapter.
//!
//! Provenance, per row:
//! - "adapter": the context window the adapter's capability table pinned
//!   before this module existed, carried over unchanged.
//! - "local": a conservative ceiling chosen here because no maximum is
//!   recorded; it never claims a provider-published API limit.
//! - "unrecorded": pinned by lane 973-output-cap from provider model
//!   documentation (Claude rows: the Anthropic models overview) without a
//!   per-row citation or check date. Confirm against the provider reference
//!   before changing, and record the source when a row is re-verified.

/// Local output ceiling for a model whose provider publishes no maximum, or
/// whose family is not in the table yet. It is deliberately above the legacy
/// 4,096-token ceiling while remaining inside the family's API maximum.
pub const UNKNOWN_OUTPUT_LIMIT: u64 = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticModelLimits {
    pub context_window: Option<u64>,
    pub max_output_tokens: u64,
}

#[must_use]
pub fn static_model_limits(provider: &str, model: &str) -> StaticModelLimits {
    let model = model.to_ascii_lowercase();
    match provider {
        "anthropic" | "anthropic-oauth" | "bedrock" | "vertex" => anthropic_limits(&model),
        "openai" | "openai-oauth" => openai_limits(&model),
        "gemini" | "google-antigravity" => gemini_limits(&model),
        "deepseek" => deepseek_limits(&model),
        // Kimi, xAI/Grok and Haider Code: no pinned context; output local.
        _ => StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
    }
}

/// Claude families, matched by substring so dated and platform (Bedrock,
/// Vertex) spellings share a row.
fn anthropic_limits(model: &str) -> StaticModelLimits {
    let million_context = [
        "fable-5",
        "mythos-5",
        "opus-5",
        "opus-4-8",
        "opus-4-7",
        "opus-4-6",
        "sonnet-5",
        "sonnet-4-6",
    ]
    .iter()
    .any(|needle| model.contains(needle));
    if million_context {
        // Context: adapter for fable-5/opus-5/sonnet-5; unrecorded for the
        // mythos and 4.6+ rows (the adapter reported 100K). Output: unrecorded.
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if model.contains("opus-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("haiku-4-5")
    {
        // Context: adapter for haiku-4-5; unrecorded for opus/sonnet 4.5
        // (the adapter reported 100K). Output: unrecorded.
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 64_000,
        }
    } else {
        // Context: adapter, conservative for an unknown Claude model rather
        // than a newer family's window. Output: local.
        StaticModelLimits {
            context_window: Some(100_000),
            max_output_tokens: 64_000,
        }
    }
}

/// OpenAI families, matched by ID prefix.
fn openai_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gpt-5.6") || model.starts_with("gpt-5.5") || model.starts_with("gpt-5.4")
    {
        // Context: adapter. Output: unrecorded.
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-5") {
        // Context: adapter. Output: unrecorded.
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-4.1") {
        // Context: adapter. Output: unrecorded.
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 32_768,
        }
    } else if model.starts_with("gpt-4o") {
        // Context: adapter. Output: unrecorded.
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 16_384,
        }
    } else if model.starts_with("o3") || model.starts_with("o4-") {
        // Context: unrecorded (the adapter reported 128K). Output: unrecorded.
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 100_000,
        }
    } else {
        // Context: adapter. Output: local.
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    }
}

/// Gemini families, matched by ID prefix.
fn gemini_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gemini-3") || model.starts_with("gemini-2.5") {
        // Context: adapter. Output: unrecorded.
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 65_536,
        }
    } else if model.starts_with("gemini-2") || model.starts_with("gemini-1.5") {
        // Context: adapter (which named 2.0 and 1.5). Output: unrecorded.
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 8_192,
        }
    } else {
        // Context: adapter. Output: local.
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 8_192,
        }
    }
}

/// Context and output: unrecorded. The V4 output ceiling is the largest in
/// the table and sets `output_budget::MAX_OUTPUT_LIMIT`.
fn deepseek_limits(model: &str) -> StaticModelLimits {
    if model.contains("v4") || model.contains("flash") {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
        }
    } else {
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 8_192,
        }
    }
}
