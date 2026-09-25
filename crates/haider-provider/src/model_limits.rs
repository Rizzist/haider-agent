//! Pinned per-model token limits: the single static source for context
//! windows and output ceilings used when a provider catalog is unavailable or
//! omits them. Catalog declarations always win at daemon projection time.
//!
//! Provenance: rows serving the subscription catalog cite the provider
//! references recorded, with their check date, in
//! `docs/subscription-model-catalog.md`. Values marked "local" are
//! conservative ceilings chosen here because the provider publishes no
//! maximum; they never claim an API limit. Rows for other families have no
//! recorded source yet.

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
        "kimi-oauth" => kimi_limits(&model),
        // Context: xAI model table, via the adapter's seed windows.
        // Output: local.
        "xai" | "grok-oauth" => StaticModelLimits {
            context_window: crate::XAI_SEED_MODEL_CONTEXT_WINDOWS
                .iter()
                .find_map(|(id, window)| (*id == model).then_some(*window)),
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
        // Context: the public Haider Code catalog declares 128K for its
        // flash row. Output: local.
        "haider-code" => StaticModelLimits {
            context_window: (model == "deepseek-v4-flash").then_some(128_000),
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
        _ => StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        },
    }
}

/// Context: Kimi Code model table. Output: local.
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
        max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
    }
}

/// Claude model table, matched by family so dated and platform (Bedrock,
/// Vertex) spellings share a row. Unmatched families get a conservative
/// window rather than a newer family's.
fn anthropic_limits(model: &str) -> StaticModelLimits {
    const MILLION_CONTEXT_FAMILIES: [&str; 7] = [
        "fable-5",
        "opus-5",
        "sonnet-5",
        "opus-4-8",
        "opus-4-7",
        "opus-4-6",
        "sonnet-4-6",
    ];
    const TWO_HUNDRED_K_FAMILIES: [&str; 3] = ["opus-4-5", "sonnet-4-5", "haiku-4-5"];
    let in_family = |families: &[&str]| families.iter().any(|family| model.contains(family));
    if in_family(&MILLION_CONTEXT_FAMILIES) {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if in_family(&TWO_HUNDRED_K_FAMILIES) {
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

/// OpenAI model cards, matched by ID prefix. The GPT-6, GPT-5.6, GPT-5.5 and
/// GPT-5.3 Codex rows serve the subscription catalog.
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
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
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
