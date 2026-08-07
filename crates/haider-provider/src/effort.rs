//! Static effort/fast capability tables for providers whose CATALOG does not
//! declare them (G3).
//!
//! Anthropic's `/v1/models` and Gemini's model list carry no effort ladder,
//! so the ladders here are pinned from the live API documentation
//! (docs/research/g-wave-external-api-research.md §"Anthropic effort +
//! thinking" / §"Anthropic fast mode", and
//! g-wave-external-api-research-2.md §"GEMINI"). Codex and Kimi pairs are
//! NOT represented here on purpose: their providers declare ladders in their
//! own catalogs, and discovery is never synthesized.
//!
//! Every gate is NAME-STATIC: an unknown model gets an EMPTY ladder and no
//! fast support, so `/effort` and `/fast` refuse honestly instead of
//! guessing.

/// Anthropic effort vocabulary for the 5-family plus opus-4-8/opus-4-7.
const ANTHROPIC_FULL_LADDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];
/// Anthropic effort vocabulary for opus-4-6/sonnet-4-6: `max` but NOT
/// `xhigh`.
const ANTHROPIC_LEGACY_LADDER: &[&str] = &["low", "medium", "high", "max"];
/// Gemini 3.x thinkingLevel vocabulary.
const GEMINI_FULL_LADDER: &[&str] = &["minimal", "low", "medium", "high"];
/// Gemini 3.1-pro: no `minimal`.
const GEMINI_PRO_LADDER: &[&str] = &["low", "medium", "high"];

/// Strips enterprise naming decorations so every spelling of a model shares
/// its family's capability row (G4b): the Bedrock `anthropic.` prefix
/// (`anthropic.claude-opus-5`), the Vertex `@YYYYMMDD` suffix
/// (`claude-sonnet-4-5@20250929`), and the first-party `-YYYYMMDD` dated
/// release suffix. Normalization is purely syntactic — an unknown family
/// still resolves to the EMPTY row, never a guess.
pub(crate) fn base_model(model: &str) -> &str {
    let model = model.strip_prefix("anthropic.").unwrap_or(model);
    let model = match model.rsplit_once('@') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()) => {
            base
        }
        _ => model,
    };
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()) => {
            base
        }
        _ => model,
    }
}

/// The effort ladder Anthropic documents for `model`. EMPTY means "no
/// documented ladder" — never a guess.
#[must_use]
pub fn anthropic_supported_efforts(model: &str) -> &'static [&'static str] {
    match base_model(model) {
        "claude-fable-5" | "claude-opus-5" | "claude-sonnet-5" | "claude-opus-4-8"
        | "claude-opus-4-7" => ANTHROPIC_FULL_LADDER,
        "claude-opus-4-6" | "claude-sonnet-4-6" => ANTHROPIC_LEGACY_LADDER,
        _ => &[],
    }
}

/// Anthropic's documented default effort (`high` == omitting the field) for
/// models with a known ladder.
#[must_use]
pub fn anthropic_default_effort(model: &str) -> Option<&'static str> {
    (!anthropic_supported_efforts(model).is_empty()).then_some("high")
}

/// Anthropic's canonical effort order, least to most.
const ANTHROPIC_EFFORT_ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Clamps a session-selected effort to `model`'s documented ladder using
/// Claude Code's published fallback rule: an unsupported level falls to the
/// HIGHEST supported level at or below it (`xhigh` on a 4.6 row → `high`).
/// A model with NO documented ladder passes the selection through VERBATIM —
/// we refuse to rewrite what we cannot know, and the provider's own error
/// surfaces. A ladder-known model with a value outside the canonical
/// vocabulary drops to `None` (provider default) rather than sending a
/// documented-invalid request.
#[must_use]
pub fn anthropic_effort_clamp(model: &str, effort: Option<&str>) -> Option<String> {
    let requested = effort?;
    let ladder = anthropic_supported_efforts(model);
    if ladder.is_empty() {
        return Some(requested.to_owned());
    }
    if ladder.contains(&requested) {
        return Some(requested.to_owned());
    }
    let requested_rank = ANTHROPIC_EFFORT_ORDER
        .iter()
        .position(|level| *level == requested)?;
    ANTHROPIC_EFFORT_ORDER[..requested_rank]
        .iter()
        .rev()
        .find(|level| ladder.contains(*level))
        .map(|level| (*level).to_owned())
}

/// Whether `model` supports the fast-mode research preview: `claude-opus-5`
/// and `claude-opus-4-8` ONLY (opus-4-7 + fast is a hard API error; opus-4-6
/// silently bills standard).
#[must_use]
pub fn anthropic_fast_mode_supported(model: &str) -> bool {
    matches!(base_model(model), "claude-opus-5" | "claude-opus-4-8")
}

/// The thinkingLevel ladder Gemini documents for `model` (3.x-named models
/// only; 2.5-era numeric `thinkingBudget` is deliberately NOT modeled).
#[must_use]
pub fn gemini_supported_efforts(model: &str) -> &'static [&'static str] {
    if model.starts_with("gemini-3.1-pro") {
        GEMINI_PRO_LADDER
    } else if model.starts_with("gemini-3") {
        GEMINI_FULL_LADDER
    } else {
        &[]
    }
}

/// Whether `model` may combine the `google_search` + `url_context` built-in
/// tools with function declarations (W-B decision 4): 3.x-named models ONLY —
/// the same name gate as the thinkingLevel ladder. 2.5-era models cannot mix
/// built-ins with function declarations, so they honestly get neither.
#[must_use]
pub fn gemini_web_builtins_supported(model: &str) -> bool {
    model.starts_with("gemini-3")
}

/// Gemini's documented default thinkingLevel for models with a known ladder
/// (`medium` on 3.6/3.5-flash, otherwise `high`).
#[must_use]
pub fn gemini_default_effort(model: &str) -> Option<&'static str> {
    if gemini_supported_efforts(model).is_empty() {
        None
    } else if model.starts_with("gemini-3.6") || model.starts_with("gemini-3.5-flash") {
        Some("medium")
    } else {
        Some("high")
    }
}
