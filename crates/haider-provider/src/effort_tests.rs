//! Laws for the static effort/fast capability tables (G3). Lives in a
//! `*_tests.rs` file per the workspace rule — inline `mod tests` modules are
//! invisible to the xtask test-count ledger.
#![allow(clippy::expect_used)]

use super::effort::*;

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
