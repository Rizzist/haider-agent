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
        selection_epoch: None,
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
    let meter = ContextMeter::resolve(None, 0, 128_000, SnapshotEpoch::Current, |window| {
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
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, SnapshotEpoch::Current, cap);
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
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, SnapshotEpoch::Previous, cap);
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
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, SnapshotEpoch::Previous, cap);
    assert_eq!(meter.turns_to_threshold, None);
    // The same model keeps it.
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 1_000_000, SnapshotEpoch::Current, cap);
    assert_eq!(meter.turns_to_threshold, Some(1_139));
}

/// After a switch to a model the catalog lists WITHOUT a window, the
/// previous model's snapshot window must not stand in for it.
#[test]
fn a_listed_model_without_a_window_ignores_the_previous_snapshot_window() {
    let snapshot = footprint(2_000, 150_000, 1_000, Some(1_000_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, SnapshotEpoch::Previous, cap);
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
        let meter = ContextMeter::resolve(Some(&snapshot), 0, window, SnapshotEpoch::Current, cap);
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
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, SnapshotEpoch::Previous, cap);
    assert_eq!(meter.window, Some(200_000));
    assert_eq!(meter.percent(), Some(77));
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(meter.threshold_projected);
    assert_eq!(meter.status_text(10), "153k tok · ▰▰▰▰▰▰▰▰▱▱ 77% of 200k");
}

#[test]
fn the_snapshot_window_serves_until_the_catalog_arrives() {
    let snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 0, SnapshotEpoch::Current, cap);
    assert_eq!(meter.window, Some(200_000));
    assert_eq!(meter.auto_compact_at, Some(170_000));
    assert!(!meter.threshold_projected);
}

#[test]
fn before_the_first_turn_the_trigger_is_projected_from_the_window() {
    let meter = ContextMeter::resolve(None, 0, 400_000, SnapshotEpoch::Current, cap);
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
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, SnapshotEpoch::Current, cap);
    assert_eq!(meter.percent(), Some(105));
}

#[test]
fn detail_defines_prompt_cached_and_reply() {
    let mut snapshot = footprint(1_200, 18_000, 400, Some(200_000), 30_000);
    snapshot.estimated_turns_to_threshold = Some(12);
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 200_000, SnapshotEpoch::Current, cap);
    assert_eq!(
        meter.detail_lines(),
        [
            "last prompt 19k (cached 18k) + reply 400",
            "auto-compact at 170k (85%)",
            "≈12 turns to auto-compaction"
        ]
    );
}

/// 973-context-meter-fixes B3 (Astra): two models with EQUAL windows but
/// different output limits. Before the new model's first snapshot the old
/// snapshot's trigger (50k, from a user-set 50k reserve) and its `≈0 turns`
/// must not read as current: the trigger is projected from the NEW epoch's
/// reservation (8,192 → min(85k, 91,808) = 85k).
///
/// MUTATION CHECK: accept the snapshot's threshold whenever its window
/// equals the displayed one (drop the epoch from `snapshot_governs`).
/// Expected runtime failure: `auto_compact_at` stays 50,000, unprojected.
#[test]
fn a_same_window_switch_invalidates_the_trigger_and_turns_estimate() {
    let mut snapshot = footprint(50_000, 0, 0, Some(100_000), 50_000);
    snapshot.estimated_turns_to_threshold = Some(0);
    assert_eq!(snapshot.soft_threshold_tokens, Some(50_000));
    // Same epoch: the daemon's figures stand.
    let meter = ContextMeter::resolve(Some(&snapshot), 0, 100_000, SnapshotEpoch::Current, cap);
    assert_eq!(meter.auto_compact_at, Some(50_000));
    assert_eq!(meter.turns_to_threshold, Some(0));
    assert!(meter.status_text(10).ends_with("compact at 50%"));
    // After the switch to the 8,192-max model (same 100k window).
    let meter = ContextMeter::resolve(
        Some(&snapshot),
        0,
        100_000,
        SnapshotEpoch::Previous,
        |window| epoch_reserved_output(None, Some(8_192), window),
    );
    assert_eq!(meter.window, Some(100_000));
    assert_eq!(meter.percent(), Some(50));
    assert_eq!(meter.auto_compact_at, Some(85_000));
    assert!(meter.threshold_projected);
    assert_eq!(meter.turns_to_threshold, None);
    assert_eq!(meter.status_text(10), "50k tok · ▰▰▰▰▰▱▱▱▱▱ 50% of 100k");
    assert_eq!(
        meter.detail_lines(),
        [
            "last prompt 50k (cached 0) + reply 0",
            "auto-compact at 85k (85%) (projected until this model's first turn)"
        ]
    );
}

/// B3: the COMMITTED effective budget (the daemon's model-selection reply)
/// beats the client's derivation — a user-set budget that fits the new
/// model survives (50k, where the derivation would say 30k), and one that
/// does not is clamped to the model's maximum.
#[test]
fn the_projection_uses_the_committed_output_budget() {
    let snapshot = footprint(40_000, 0, 0, Some(100_000), 50_000);
    let project = |committed: Option<u64>, declared: Option<u64>| {
        ContextMeter::resolve(
            Some(&snapshot),
            0,
            100_000,
            SnapshotEpoch::Previous,
            |window| epoch_reserved_output(committed, declared, window),
        )
        .auto_compact_at
    };
    // User-set 50k kept on a 60k-max model: min(85k, 100k − 50k).
    assert_eq!(project(Some(50_000), Some(60_000)), Some(50_000));
    // Without the committed figure the derivation (30k) would claim 70k.
    assert_eq!(project(None, Some(60_000)), Some(70_000));
    // User-set 50k clamped to an 8,192-max model.
    assert_eq!(project(Some(8_192), Some(8_192)), Some(85_000));
}

/// Carried follow-up (Astra/final review): the different-window projected
/// arm uses the NEW model's reservation, not the previous snapshot's:
/// 400k/30k → 128k with a 16,384 maximum projects 108,800 (not 98k).
#[test]
fn a_window_switch_projects_with_the_new_models_reserve() {
    let snapshot = footprint(3_000, 100_000, 1_000, Some(400_000), 30_000);
    let meter = ContextMeter::resolve(
        Some(&snapshot),
        0,
        128_000,
        SnapshotEpoch::Previous,
        |window| epoch_reserved_output(None, Some(16_384), window),
    );
    assert_eq!(meter.auto_compact_at, Some(108_800));
    assert!(meter.threshold_projected);
}

/// Epoch admission uses the daemon's version, including for equal values.
#[test]
fn meter_epoch_transitions() {
    let pair = |model: &str| ("p".to_owned(), model.to_owned());
    let mut old = footprint(10_000, 0, 0, Some(100_000), 30_000);
    old.selection_epoch = Some(1);
    let mut epoch = MeterEpoch::default();
    assert!(epoch.admit(pair("a"), Some(1), Some(30_000), Some(false)));
    assert_eq!(epoch.snapshot_epoch(Some(&old)), SnapshotEpoch::Current);
    assert!(epoch.admit(pair("b"), Some(2), Some(8_192), Some(true)));
    assert_eq!(epoch.snapshot_epoch(Some(&old)), SnapshotEpoch::Previous);
    assert_eq!(epoch.output_budget, Some(8_192));
    assert!(!epoch.admit(pair("a"), Some(1), Some(30_000), Some(false)));
    let mut other = MeterEpoch::default();
    other.admit(pair("a"), Some(1), Some(30_000), Some(false));
    other.admit(pair("b"), Some(2), None, None);
    other.admit(pair("b"), Some(2), Some(8_192), Some(true));
    assert_eq!(other, epoch);
    let mut fresh = footprint(12_000, 0, 0, Some(100_000), 8_192);
    fresh.selection_epoch = Some(2);
    assert_eq!(epoch.snapshot_epoch(Some(&fresh)), SnapshotEpoch::Current);
    // The equal-epoch request proves its window, while the mismatched
    // reserve cannot lend this epoch its trigger or turns.
    let mut identical = old.clone();
    identical.selection_epoch = Some(2);
    assert_eq!(
        epoch.snapshot_epoch(Some(&identical)),
        SnapshotEpoch::CurrentReserveMismatch
    );
}

#[test]
fn epochless_legacy_metadata_confirms_budget_without_claiming_snapshot_truth() {
    let pair = ("local".to_owned(), "model".to_owned());
    let mut epoch = MeterEpoch::default();
    assert!(epoch.admit(pair.clone(), Some(2), Some(50_000), Some(true)));
    assert!(epoch.admit(pair.clone(), Some(3), None, None));
    assert!(epoch.reserve_assumed);
    assert!(epoch.admit(pair.clone(), None, Some(30_000), Some(true)));
    assert_eq!(epoch.selection_epoch, Some(3));
    assert_eq!(epoch.output_budget, Some(30_000));
    assert!(!epoch.reserve_assumed);
    let mut old = footprint(10_000, 0, 0, Some(100_000), 30_000);
    old.selection_epoch = None;
    assert_eq!(epoch.snapshot_epoch(Some(&old)), SnapshotEpoch::Previous);
    let mut meter = ContextMeter::resolve(
        Some(&old),
        0,
        100_000,
        epoch.snapshot_epoch(Some(&old)),
        |window| epoch_reserved_output(epoch.output_budget, None, window),
    );
    meter.reserve_assumed = epoch.reserve_assumed;
    assert!(
        meter
            .detail_lines()
            .iter()
            .all(|line| !line.contains("pending confirmation"))
    );
    assert!(!epoch.admit(("other".into(), "model".into()), None, None, None));
}
