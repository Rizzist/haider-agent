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

/// Strips a trailing `-YYYYMMDD` date suffix so dated release slugs share
/// their family's capability row.
fn base_model(model: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// LAW (LE2, static-table half): the anthropic ladders are exactly the
    /// documented families — xhigh present on the 5-family/4.8/4.7 rows,
    /// absent on the 4.6 rows, and unknown models get an EMPTY ladder.
    #[test]
    fn anthropic_static_ladders_match_the_documented_families() {
        for model in [
            "claude-fable-5",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-8-20260115",
        ] {
            assert_eq!(
                anthropic_supported_efforts(model),
                ["low", "medium", "high", "xhigh", "max"],
                "full ladder for {model}"
            );
            assert_eq!(anthropic_default_effort(model), Some("high"));
        }
        for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            assert_eq!(
                anthropic_supported_efforts(model),
                ["low", "medium", "high", "max"],
                "max-not-xhigh ladder for {model}"
            );
        }
        for model in ["claude-haiku-4-5", "claude-opus-5-1", "not-a-claude"] {
            assert!(
                anthropic_supported_efforts(model).is_empty(),
                "unknown model {model} must get an empty ladder"
            );
            assert_eq!(anthropic_default_effort(model), None);
        }
    }

    /// LAW (review of record): the clamp follows Claude Code's published
    /// fallback rule — an unsupported level falls to the HIGHEST supported
    /// level at or below it; supported levels and ladder-unknown models pass
    /// verbatim; out-of-vocabulary values on a known ladder drop to `None`.
    #[test]
    fn anthropic_effort_clamp_falls_down_the_documented_ladder() {
        assert_eq!(
            anthropic_effort_clamp("claude-opus-4-6", Some("xhigh")).as_deref(),
            Some("high"),
            "xhigh -> high on the max-not-xhigh ladder (max is ABOVE xhigh)"
        );
        assert_eq!(
            anthropic_effort_clamp("claude-sonnet-4-6", Some("max")).as_deref(),
            Some("max"),
            "max is IN the 4.6 ladder and passes"
        );
        assert_eq!(
            anthropic_effort_clamp("claude-fable-5", Some("xhigh")).as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            anthropic_effort_clamp("claude-mystery-9", Some("xhigh")).as_deref(),
            Some("xhigh"),
            "no documented ladder: pass through verbatim"
        );
        assert_eq!(anthropic_effort_clamp("claude-opus-5", Some("turbo")), None);
        assert_eq!(anthropic_effort_clamp("claude-opus-5", None), None);
    }

    /// LAW (LE4, gate half): fast mode is statically gated to claude-opus-5
    /// and claude-opus-4-8 (dated slugs included); opus-4-7 and opus-4-6 are
    /// OUT — 4.7 hard-errors and 4.6 silently bills standard.
    #[test]
    fn anthropic_fast_gate_is_opus5_and_opus48_only() {
        assert!(anthropic_fast_mode_supported("claude-opus-5"));
        assert!(anthropic_fast_mode_supported("claude-opus-4-8"));
        assert!(anthropic_fast_mode_supported("claude-opus-4-8-20260115"));
        assert!(!anthropic_fast_mode_supported("claude-opus-4-7"));
        assert!(!anthropic_fast_mode_supported("claude-opus-4-6"));
        assert!(!anthropic_fast_mode_supported("claude-sonnet-5"));
        assert!(!anthropic_fast_mode_supported("claude-fable-5"));
        assert!(!anthropic_fast_mode_supported("claude-opus-5-1"));
    }

    /// LAW (LE2, gemini half): 3.x names get the documented thinkingLevel
    /// ladder (3.1-pro without `minimal`); non-3.x models get NOTHING —
    /// 2.5-era numeric budgets are deliberately unmodeled.
    #[test]
    fn gemini_static_ladders_are_name_gated_to_3x() {
        assert_eq!(
            gemini_supported_efforts("gemini-3-flash"),
            ["minimal", "low", "medium", "high"]
        );
        assert_eq!(
            gemini_supported_efforts("gemini-3.1-pro"),
            ["low", "medium", "high"]
        );
        assert_eq!(gemini_default_effort("gemini-3.1-pro"), Some("high"));
        assert_eq!(gemini_default_effort("gemini-3.6-flash"), Some("medium"));
        assert_eq!(gemini_default_effort("gemini-3.5-flash"), Some("medium"));
        assert!(gemini_supported_efforts("gemini-2.5-pro").is_empty());
        assert_eq!(gemini_default_effort("gemini-2.5-flash"), None);
    }
}
