//! Pinned token limits used when provider catalogs are unavailable or omit
//! limits. Catalog declarations always win at daemon projection time.

/// Conservative provider fallback for a model family whose exact row is not
/// yet in the pinned table. Every fallback is deliberately above the legacy
/// 4,096-token ceiling while remaining inside the family's established API
/// maximum.
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
        "kimi-oauth" => kimi_limits(&model),
        "xai" | "grok-oauth" => StaticModelLimits {
            context_window: crate::XAI_SEED_MODEL_CONTEXT_WINDOWS
                .iter()
                .find_map(|(id, window)| (*id == model).then_some(*window)),
            max_output_tokens: 32_768,
        },
        "haider-code" => StaticModelLimits {
            // The public Haider Code catalog declares 128K for its flash row.
            context_window: (model == "deepseek-v4-flash").then_some(128_000),
            max_output_tokens: 32_768,
        },
        _ => StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
    }
}

fn kimi_limits(model: &str) -> StaticModelLimits {
    let context_window = match model {
        // K3's 1M window depends on membership; advertise the safe common
        // limit until a credential-specific catalog says otherwise.
        "k3" | "k3-256k" | "kimi-for-coding-highspeed" => Some(262_144),
        "kimi-for-coding" => Some(1_048_576),
        _ => None,
    };
    StaticModelLimits {
        context_window,
        max_output_tokens: 32_768,
    }
}

fn anthropic_limits(model: &str) -> StaticModelLimits {
    let current_long = [
        "fable-5",
        "opus-5",
        "sonnet-5",
        "opus-4-8",
        "opus-4-7",
        "opus-4-6",
        "sonnet-4-6",
    ]
    .iter()
    .any(|needle| model.contains(needle));
    if current_long {
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
            context_window: Some(100_000),
            max_output_tokens: 64_000,
        }
    }
}

fn openai_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gpt-6-") || model.starts_with("gpt-5.6") || model.starts_with("gpt-5.5") {
        StaticModelLimits {
            context_window: Some(1_050_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-5.3-codex") {
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-5") {
        StaticModelLimits {
            context_window: None,
            max_output_tokens: 128_000,
        }
    } else if model.starts_with("gpt-4.1") {
        StaticModelLimits {
            context_window: Some(1_047_576),
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
            context_window: None,
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
    } else if model.starts_with("gemini-2") {
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 8_192,
        }
    } else {
        StaticModelLimits {
            context_window: None,
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
            context_window: None,
            max_output_tokens: 8_192,
        }
    }
}
