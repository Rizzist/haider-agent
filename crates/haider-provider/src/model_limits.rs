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
        "kimi-oauth" => kimi_code_limits(&model),
        "xai" | "grok-oauth" => xai_limits(&model),
        "haider-code" => haider_code_limits(&model),
        // Local synthetic adapter, not a provider API claim: its fixture
        // accepts the shared default budget and keeps durable test goldens
        // representative of the ordinary 30,000-token path.
        "fake" => StaticModelLimits {
            context_window: None,
            max_output_tokens: haider_protocol::output_budget::DEFAULT_OUTPUT_LIMIT,
        },
        // Custom profiles and unlisted adapters: unverified local fallback.
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

/// Current DeepSeek IDs, checked 2026-09-24:
/// https://api-docs.deepseek.com/quick_start/pricing ("CONTEXT LENGTH 1M",
/// "MAX OUTPUT MAXIMUM: 384K") and
/// https://api-docs.deepseek.com/api/create-chat-completion (`max_tokens`
/// "must be between 1 and 384K (393216)"; model enum `deepseek-flash`,
/// `deepseek-v4-pro`). `deepseek-flash` is DeepSeek-V4.1-Flash; the legacy
/// `deepseek-v4-flash*` names are served by it. 384,000 stays below the
/// documented 393,216 ceiling.
fn deepseek_limits(model: &str) -> StaticModelLimits {
    let current = model == "deepseek-flash"
        || model.starts_with("deepseek-flash-")
        || model.split(['-', '_', '.']).any(|part| part == "v4");
    if current {
        StaticModelLimits {
            context_window: Some(1_000_000),
            max_output_tokens: 384_000,
        }
    } else {
        // Unverified: `deepseek-chat`/`deepseek-reasoner` were retired
        // 2026-07-24 and no longer have documented limits. No context claim;
        // the daemon can use a discovered provider limit instead.
        StaticModelLimits {
            context_window: None,
            max_output_tokens: UNKNOWN_OUTPUT_LIMIT,
        }
    }
}

/// Kimi Code subscription models (`https://api.kimi.com/coding/v1`), checked
/// 2026-09-24 at https://www.kimi.com/code/docs/en/kimi-code/models.html:
/// `k3` 1,048,576 on Pro plans and 262,144 otherwise (the plan-independent
/// 262,144 is used), `k3-256k` 262,144, `kimi-for-coding` 1,048,576,
/// `kimi-for-coding-highspeed` 262,144. Kimi publishes no separate output
/// ceiling; its documented constraint is input + output <= context
/// (https://platform.kimi.ai/docs/api/chat.md), so the shared default budget
/// is the output maximum — well inside every documented window.
fn kimi_code_limits(model: &str) -> StaticModelLimits {
    let context_window = match model {
        "kimi-for-coding" => Some(1_048_576),
        "k3" | "k3-256k" | "kimi-for-coding-highspeed" | "kimi-k3" => Some(262_144),
        "kimi-k2.7-code" | "kimi-k2.7-code-highspeed" | "kimi-k2.6" => Some(262_144),
        _ => None,
    };
    StaticModelLimits {
        context_window,
        max_output_tokens: if context_window.is_some() {
            crate::output_budget::DEFAULT_OUTPUT_LIMIT
        } else {
            UNKNOWN_OUTPUT_LIMIT
        },
    }
}

/// xAI Grok models, checked 2026-09-24 at https://docs.x.ai/docs/models
/// (grok-4.7/4.6/4.5 500,000; grok-4.3 and grok-4.20 1,000,000;
/// grok-build-0.1, alias grok-code-fast-1, 256,000; grok-build-latest is an
/// alias of grok-4.5). Output: xAI documents `max_completion_tokens` /
/// `max_output_tokens` "Defaults to 128,000 when unset"
/// (https://docs.x.ai/developers/rest-api-reference/inference/chat-completions.md),
/// so 128,000 is an accepted per-response budget. The Grok subscription proxy
/// serves the same model IDs.
fn xai_limits(model: &str) -> StaticModelLimits {
    let context_window = if ["grok-4.7", "grok-4.6", "grok-4.5", "grok-build-latest"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        Some(500_000)
    } else if model.starts_with("grok-4.3") || model.starts_with("grok-4.20") {
        Some(1_000_000)
    } else if model.starts_with("grok-build-0.1") || model.starts_with("grok-code-fast") {
        Some(256_000)
    } else {
        None
    };
    StaticModelLimits {
        context_window,
        max_output_tokens: if context_window.is_some() {
            128_000
        } else {
            UNKNOWN_OUTPUT_LIMIT
        },
    }
}

/// Haider Code hosted models, from the public catalog
/// https://haidercode.ai/v1/models (read 2026-09-24 without credentials; see
/// also https://haidercode.ai/docs/go: "Context is the window the model
/// publishes"). The catalog publishes `max_output_tokens: null` for every
/// model, so the shared default budget is the output maximum for rows with a
/// published window; rows with an unpublished window stay on the unverified
/// fallback. The live catalog's declarations win at projection time.
fn haider_code_limits(model: &str) -> StaticModelLimits {
    let context_window = match model {
        "deepseek-v4-flash" | "deepseek-v4-pro" => Some(128_000),
        "glm-5.3-flash" | "glm-5.2" | "glm-5.3" => Some(200_000),
        "qwen3.7-plus" | "kimi-k2.7-code" => Some(262_144),
        "qwen3.8-27b" | "qwen3.8-2.4t-a95b" | "qwen3.8-max" | "kimi-k3" | "minimax-m3" => {
            Some(1_000_000)
        }
        _ => None,
    };
    StaticModelLimits {
        context_window,
        max_output_tokens: if context_window.is_some() {
            crate::output_budget::DEFAULT_OUTPUT_LIMIT
        } else {
            UNKNOWN_OUTPUT_LIMIT
        },
    }
}
