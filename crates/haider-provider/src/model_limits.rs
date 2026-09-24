//! Pinned token limits used when provider catalogs are unavailable or omit
//! limits. Catalog declarations always win at daemon projection time.

/// Conservative provider fallback for a model family whose exact row is not
/// yet in the pinned table. Every fallback is deliberately above the legacy
/// 4,096-token ceiling while remaining inside the family's established API
/// maximum.
pub const UNKNOWN_OUTPUT_LIMIT: u64 = 32_768;
/// Default requested by interactive/headless clients after model resolution.
pub const DEFAULT_OUTPUT_LIMIT: u64 = 30_000;
/// Largest response budget supported by any currently registered adapter.
/// Daemon admission applies this after catalog/model-specific resolution.
pub const MAX_OUTPUT_LIMIT: u64 = 384_000;

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
        "kimi-oauth" => StaticModelLimits {
            context_window: None,
            max_output_tokens: 32_768,
        },
        "xai" | "grok-oauth" | "haider-code" => StaticModelLimits {
            context_window: None,
            max_output_tokens: 32_768,
        },
        _ => StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
    }
}

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
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if model.contains("opus-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("haiku-4-5")
    {
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 64_000,
        }
    } else {
        StaticModelLimits {
            // Match the adapter's conservative capability for an unknown
            // Anthropic model rather than inventing the 5-family window.
            context_window: Some(100_000),
            max_output_tokens: 64_000,
        }
    }
}

fn openai_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gpt-5.6") || model.starts_with("gpt-5.5") || model.starts_with("gpt-5.4")
    {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-5") {
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-4.1") {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 32_768,
        }
    } else if model.starts_with("gpt-4o") {
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 16_384,
        }
    } else if model.starts_with("o3") || model.starts_with("o4-") {
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 100_000,
        }
    } else {
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 32_768,
        }
    }
}

fn gemini_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gemini-3") || model.starts_with("gemini-2.5") {
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 65_536,
        }
    } else if model.starts_with("gemini-2") || model.starts_with("gemini-1.5") {
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 8_192,
        }
    } else {
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 8_192,
        }
    }
}

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
