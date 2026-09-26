#![allow(clippy::expect_used)]

use super::*;

fn footprint(
    input: u64,
    cached: u64,
    output: u64,
    window: Option<u64>,
    reserved: u64,
) -> ContextFootprint {
    ContextFootprint {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        used_tokens: input + cached + output,
        context_window: window,
        reserved_output_tokens: reserved,
        soft_threshold_tokens: window.and_then(|window| {
            haider_protocol::context::context_soft_threshold_tokens(window, reserved)
        }),
        estimated_turns_to_threshold: None,
        truth: ContextFootprintTruth::Exact,
        accounting: None,
    }
}

fn cap(window: u64) -> u64 {
    derived_reserved_output(None, window)
}

#[test]
fn the_projected_reservation_follows_the_derived_output_budget() {
    assert_eq!(derived_reserved_output(None, 200_000), 30_000);
    assert_eq!(derived_reserved_output(Some(64_000), 200_000), 30_000);
    assert_eq!(derived_reserved_output(Some(8_192), 128_000), 8_192);
    assert_eq!(derived_reserved_output(None, 16_000), 16_000);
    assert_eq!(derived_reserved_output(Some(8_192), 0), 8_192);
    // A small-max model projects its trigger at 85% (hard fit is far).
    let meter = ContextMeter::resolve(None, 0, 128_000, true, |window| {
        derived_reserved_output(Some(8_192), window)
    });
    assert_eq!(meter.auto_compact_at, Some(108_800));
}

/// The owner's bug: an output budget standing in for the window. The
/// profile seed (4,096) must never be used as a window; a live model with
/// no declared window reads UNKNOWN, not 100%.
#[test]
fn regression_an_unknown_window_never_reads_full() {
    let snapshot = footprint(1_200, 18_000, 400, None, 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, true, cap);
    assert_eq!(meter.used_tokens, 19_600);
    assert_eq!(meter.window, None);
    assert_eq!(meter.percent(), None);
    assert_eq!(meter.status_text(10), "20k tok · window unknown");
    assert!(!meter.status_text(10).contains("100%"));
}

/// 973-context-meter verify HOLD: a switch to an unknown-window model
/// before its first turn must not keep the previous model's turns
/// estimate (it was `≈1139 turns` from a 1M window).
#[test]
fn a_switch_to_an_unknown_window_drops_the_previous_turns_estimate() {
    let mut snapshot = footprint(2_700, 50_000, 700, Some(1_000_000), 30_000);
    snapshot.estimated_turns_to_threshold = Some(1_139);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, false, cap);
    assert_eq!(meter.window, None);
    assert_eq!(meter.turns_to_threshold, None);
    assert!(
        !meter
            .detail_lines()
            .iter()
            .any(|line| line.contains("turns")),
        "{:?}",
        meter.detail_lines()
    );
    // Nor across a switch to a KNOWN window (the estimate was measured
    // against the old window).
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
    assert_eq!(meter.turns_to_threshold, None);
    // The same model keeps it.
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 1_000_000, true, cap);
    assert_eq!(meter.turns_to_threshold, Some(1_139));
}

/// After a switch to a model the catalog lists WITHOUT a window, the
/// previous model's snapshot window must not stand in for it.
#[test]
fn a_listed_model_without_a_window_ignores_the_previous_snapshot_window() {
    let snapshot = footprint(2_000, 150_000, 1_000, Some(1_000_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, false, cap);
    assert_eq!(meter.window, None);
    assert_eq!(meter.auto_compact_at, None);
    assert_eq!(meter.status_text(10), "153k tok · window unknown");
}

#[test]
fn per_model_windows_and_percentages() {
    // (model, window, prompt uncached, cached, reply, percent, trigger %)
    let cases: [(&str, u64, u64, u64, u64, u64, u64); 6] = [
        ("claude-opus-5-5", 1_000_000, 2_000, 150_000, 1_000, 15, 85),
        ("claude-sonnet-4-6", 200_000, 1_200, 18_000, 400, 10, 85),
        ("gpt-5.6-sol", 272_000, 5_000, 40_000, 2_000, 17, 85),
        ("gpt-6-sol", 400_000, 3_000, 120_000, 1_500, 31, 85),
        ("gemini-3-pro", 1_048_576, 10_000, 0, 500, 1, 85),
        // A 128k model: the output reservation (30k) caps the trigger
        // below 85% — the hard fit wins (98k = 77%).
        ("deepseek-v4-flash", 128_000, 60_000, 30_000, 1_000, 71, 77),
    ];
    for (model, window, input, cached, reply, percent, trigger) in cases {
        let snapshot = footprint(input, cached, reply, Some(window), cap(window));
        let meter = ContextMeter::resolve(Some(&snapshot), 0, window, true, cap);
        assert_eq!(meter.window, Some(window), "{model}");
        assert_eq!(meter.prompt_tokens, Some(input + cached), "{model}");
        assert_eq!(meter.percent(), Some(percent), "{model}");
        assert_eq!(meter.auto_compact_percent(), Some(trigger), "{model}");
        assert!(!meter.threshold_projected, "{model}");
    }
}

#[test]
fn a_model_switch_rebases_window_and_projects_the_new_trigger() {
    // Snapshot taken on a 1M model; the user switched to a 200k model.
    let snapshot = footprint(2_000, 150_000, 1_000, Some(1_000_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
    assert_eq!(meter.window, Some(200_000));
    assert_eq!(meter.percent(), Some(77));
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(meter.threshold_projected);
    assert_eq!(meter.status_text(10), "153k tok · ▰▰▰▰▰▰▰▰▱▱ 77% of 200k");
}

#[test]
fn the_snapshot_window_serves_until_the_catalog_arrives() {
    let snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, true, cap);
    assert_eq!(meter.window, Some(200_000));
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(!meter.threshold_projected);
}

#[test]
fn before_the_first_turn_the_trigger_is_projected_from_the_window() {
    let meter = ContextMeter::resolve(None, 0, 400_000, true, cap);
    assert_eq!(meter.status_text(10), "0 tok · ▱▱▱▱▱▱▱▱▱▱ 0% of 400k");
    assert_eq!(meter.auto_compact_at, Some(340_000));
    assert!(meter.threshold_projected);
    assert_eq!(
        meter.detail_lines(),
        [
            "no request snapshot yet",
            "auto-compact at 340k (85%) (projected until this model's first turn)"
        ]
    );
}

#[test]
fn an_overfull_context_reads_over_one_hundred_percent() {
    let snapshot = footprint(210_000, 0, 0, Some(200_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
    assert_eq!(meter.percent(), Some(105));
}

#[test]
fn detail_defines_prompt_cached_and_reply() {
    let mut snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
    snapshot.estimated_turns_to_threshold = Some(12);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, true, cap);
    assert_eq!(
        meter.detail_lines(),
        [
            "last prompt 19k (cached 18k) + reply 400",
            "auto-compact at 170k (85%)",
            "≈12 turns to auto-compaction"
        ]
    );
}
