//! Pinned per-model token limits: the static source for context windows and
//! output ceilings used when a provider catalog is unavailable or omits them.
//! Catalog declarations always win at daemon projection time
//! (`output_budget::model_output_limit`). Adapter capability tables
//! (`anthropic.rs`, `openai.rs`, `gemini.rs`) read their context windows from
//! here, so a row change moves both the daemon projection and the adapter.
//!
//! Official provider references are linked at each verified row. Unverified
//! family fallbacks use small ceilings and do not assert a provider limit.

/// Unverified local output fallback for a model whose provider publishes no
/// maximum, or whose family is not in the table yet. It remains below the
/// current known family maxima without claiming a provider guarantee.
pub const UNKNOWN_OUTPUT_LIMIT: u64 = 8_192;

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
        // Local synthetic adapter, not a provider API claim: its fixture
        // accepts the shared default budget and keeps durable test goldens
        // representative of the ordinary 30,000-token path.
        "fake" => StaticModelLimits {
            context_window: None,
            max_output_tokens: haider_protocol::output_budget::DEFAULT_OUTPUT_LIMIT,
        },
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
        // https://platform.claude.com/docs/en/build-with-claude/context-windows
        // This covers Mythos 5, Opus 4.6+, Sonnet 4.6+, and the 5 families.
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 128_000,
        }
    } else if model.contains("opus-4-5")
        || model.contains("sonnet-4-5")
        || model.contains("haiku-4-5")
    {
        // https://platform.claude.com/docs/en/models/opus-4-5/overview
        // https://platform.claude.com/docs/en/models/sonnet-4-5/overview
        // https://platform.claude.com/docs/en/models/overview
        // https://platform.claude.com/docs/en/build-with-claude/context-windows
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 64_000,
        }
    } else {
        // Unverified older/unknown Claude: the adapter uses its conservative
        // local fallback; the public table makes no context claim.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    }
}

/// Documented OpenAI IDs and their dated snapshots. Unknown aliases use the
/// unverified local fallback instead of inheriting a larger context window.
fn openai_limits(model: &str) -> StaticModelLimits {
    if documented_openai_id(model, "gpt-5.6-cyber") {
        // https://developers.openai.com/api/docs/models/gpt-5.6-cyber
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if ["gpt-5-chat-latest", "gpt-5.3-chat-latest"]
        .iter()
        .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-5-chat-latest
        // https://developers.openai.com/api/docs/models/gpt-5.3-chat-latest
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 16_384,
        }
    } else if ["gpt-5.4-mini", "gpt-5.4-nano"]
        .iter()
        .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-5.4-mini
        // https://developers.openai.com/api/docs/models/gpt-5.4-nano
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if [
        "gpt-5.6",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.5-pro",
        "gpt-5.4",
        "gpt-5.4-pro",
    ]
    .iter()
    .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-5.6-sol
        // https://developers.openai.com/api/docs/models/gpt-5.6-terra
        // https://developers.openai.com/api/docs/models/gpt-5.6-luna
        // https://developers.openai.com/api/docs/models/gpt-5.5
        // https://developers.openai.com/api/docs/models/gpt-5.5-pro
        // https://developers.openai.com/api/docs/models/gpt-5.4
        // https://developers.openai.com/api/docs/models/gpt-5.4-pro
        StaticModelLimits {
            context_window: Some(1_050_000),
            max_output_tokens: 128_000,
        }
    } else if [
        "gpt-5",
        "gpt-5-mini",
        "gpt-5-nano",
        "gpt-5-codex",
        "gpt-5.1",
        "gpt-5.2",
        "gpt-5.2-pro",
        "gpt-5.3-codex",
    ]
    .iter()
    .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-5
        // https://developers.openai.com/api/docs/models/gpt-5-mini
        // https://developers.openai.com/api/docs/models/gpt-5-nano
        // https://developers.openai.com/api/docs/models/gpt-5-codex
        // https://developers.openai.com/api/docs/models/gpt-5.1
        // https://developers.openai.com/api/docs/models/gpt-5.2
        // https://developers.openai.com/api/docs/models/gpt-5.2-pro
        // https://developers.openai.com/api/docs/models/gpt-5.3-codex
        // Other aliases fall through to the unverified local fallback;
        // discovery can supply a model-specific limit.
        StaticModelLimits {
            context_window: Some(400_000),
            max_output_tokens: 128_000,
        }
    } else if ["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano"]
        .iter()
        .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-4.1
        // https://developers.openai.com/api/docs/models/gpt-4.1-mini
        // https://developers.openai.com/api/docs/models/gpt-4.1-nano
        StaticModelLimits {
            context_window: Some(1_047_576),
            max_output_tokens: 32_768,
        }
    } else if ["gpt-4o", "gpt-4o-mini"]
        .iter()
        .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/gpt-4o
        // https://developers.openai.com/api/docs/models/gpt-4o-mini
        StaticModelLimits {
            context_window: Some(128_000),
            max_output_tokens: 16_384,
        }
    } else if ["o3", "o3-mini", "o3-pro", "o4-mini"]
        .iter()
        .any(|id| documented_openai_id(model, id))
    {
        // https://developers.openai.com/api/docs/models/o3
        // https://developers.openai.com/api/docs/models/o3-mini
        // https://developers.openai.com/api/docs/models/o3-pro
        // https://developers.openai.com/api/docs/models/o4-mini
        StaticModelLimits {
            context_window: Some(200_000),
            max_output_tokens: 100_000,
        }
    } else {
        // Unverified unknown OpenAI model: no context claim, local output fallback.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    }
}

fn documented_openai_id(model: &str, id: &str) -> bool {
    model == id
        || model
            .strip_prefix(id)
            .is_some_and(|suffix| suffix.starts_with("-20"))
}

/// Gemini families, matched by ID prefix.
fn gemini_limits(model: &str) -> StaticModelLimits {
    if model.starts_with("gemini-3-pro-image") {
        // https://ai.google.dev/gemini-api/docs/models/gemini-3-pro-image
        StaticModelLimits {
            context_window: Some(65_536),
            max_output_tokens: 32_768,
        }
    } else if model.contains("-image")
        || model.contains("-tts")
        || model.contains("-live")
        || model.contains("-audio")
    {
        // Unverified multimodal variant: text-family windows may be unsafe.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    } else if model.starts_with("gemini-3.5-flash")
        || model.starts_with("gemini-3-flash-preview")
        || model.starts_with("gemini-2.5")
    {
        // https://ai.google.dev/gemini-api/docs/models/gemini-2.5-pro
        // https://ai.google.dev/gemini-api/docs/models/gemini-2.5-flash-lite
        // https://ai.google.dev/gemini-api/docs/models/gemini-3.5-flash
        // https://ai.google.dev/gemini-api/docs/models/gemini-3-flash-preview
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 65_536,
        }
    } else if model.starts_with("gemini-2.0") || model.starts_with("gemini-1.5") {
        // https://ai.google.dev/gemini-api/docs/models/gemini-2.0-flash
        // https://ai.google.dev/gemini-api/docs/models/gemini
        StaticModelLimits {
            context_window: Some(1_048_576),
            max_output_tokens: 8_192,
        }
    } else {
        // Unverified unknown Gemini model: conservative local fallback.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    }
}

/// V4 values: https://api-docs.deepseek.com/quick_start/pricing/
fn deepseek_limits(model: &str) -> StaticModelLimits {
    if model.split(['-', '_', '.']).any(|part| part == "v4") {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
        }
    } else {
        // Unverified older model: no context claim; the daemon can use a
        // discovered provider limit without risking an oversized request.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: 8_192,
        }
    }
}
