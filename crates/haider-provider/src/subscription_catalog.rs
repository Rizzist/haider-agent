//! Maintained model IDs for subscription providers whose credential may run
//! turns while its `/models` request is refused.
//!
//! These rows are a floor, not the catalog: the daemon merges them with any
//! remote discovery, and remote metadata wins for a matching ID. Per-model
//! limits come from [`crate::static_model_limits`]; this module holds IDs and
//! their picker order only. Each list's source URL and check date are in
//! `docs/subscription-model-catalog.md`; update both together.

use crate::catalog::DiscoveredModel;

/// From Anthropic's Claude models overview.
const ANTHROPIC_OAUTH_IDS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
    "claude-opus-5",
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
];

/// From OpenAI's models reference.
const OPENAI_OAUTH_IDS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.6",
    "gpt-5.5",
    "gpt-5.3-codex",
];

/// From Kimi Code's models page.
const KIMI_OAUTH_IDS: &[&str] = &[
    "kimi-for-coding",
    "k3",
    "k3-256k",
    "kimi-for-coding-highspeed",
];

/// From xAI's models reference.
const GROK_OAUTH_IDS: &[&str] = &["grok-4.6", "grok-4.5", "grok-4.3"];

/// From Haider Code's public `/v1/models` response.
const HAIDER_CODE_IDS: &[&str] = &["deepseek-v4-flash"];

/// The maintained IDs for `provider`, in picker order; empty for providers
/// without a subscription fallback.
#[must_use]
pub fn subscription_static_model_ids(provider: &str) -> &'static [&'static str] {
    match provider {
        crate::ANTHROPIC_OAUTH_PROVIDER_NAME => ANTHROPIC_OAUTH_IDS,
        crate::OPENAI_OAUTH_PROVIDER_NAME => OPENAI_OAUTH_IDS,
        crate::KIMI_OAUTH_PROVIDER_NAME => KIMI_OAUTH_IDS,
        crate::GROK_OAUTH_PROVIDER_NAME => GROK_OAUTH_IDS,
        crate::HAIDER_CODE_PROVIDER_NAME => HAIDER_CODE_IDS,
        _ => &[],
    }
}

/// Whether `provider` has maintained rows that stay selectable without a
/// successful remote list.
#[must_use]
pub fn has_subscription_static_catalog(provider: &str) -> bool {
    !subscription_static_model_ids(provider).is_empty()
}

/// The maintained rows for `provider`, with limits from the static table and
/// priority equal to list position.
#[must_use]
pub fn subscription_static_models(provider: &str) -> Vec<DiscoveredModel> {
    subscription_static_model_ids(provider)
        .iter()
        .enumerate()
        .map(|(priority, id)| static_row(provider, id, priority as i64))
        .collect()
}

fn static_row(provider: &str, id: &str, priority: i64) -> DiscoveredModel {
    DiscoveredModel {
        slug: id.to_owned(),
        display_name: id.to_owned(),
        context_window: crate::static_model_limits(provider, id).context_window,
        description: None,
        default_effort: None,
        supported_efforts: Vec::new(),
        visible: true,
        priority: Some(priority),
        extensions: None,
    }
}
