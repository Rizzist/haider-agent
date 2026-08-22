//! W-flow — the token panel reports cache health as TWO numbers, because
//! either alone misleads: the lifetime ratio counts the unavoidable first
//! send of new content as a miss, and the re-read rate hides real cold-start
//! cost. `None` is not zero.

#![allow(clippy::expect_used)]

use haider_protocol::agent::AgentUsageMetrics;

fn usage(lifetime: Option<u32>, reread: Option<u32>) -> AgentUsageMetrics {
    AgentUsageMetrics {
        cache_hit_basis_points: lifetime,
        cache_reread_hit_basis_points: reread,
        ..AgentUsageMetrics::default()
    }
}

/// MUTATION CHECK (executed): render `cache_reread_hit_basis_points` as 0%
/// when absent (collapse the `(Some, None)` arm into the two-number arm with
/// `unwrap_or_default()`). Expected RUNTIME failure: the n/a assertion — a
/// session with nothing to re-read would advertise total cache failure.
#[test]
fn an_absent_reread_rate_renders_na_never_zero() {
    let line = haider_tui::agent_metrics::cache_line(&usage(Some(7_190), None));
    assert!(
        line.contains("re-read n/a"),
        "nothing to re-read must say so: {line}"
    );
    assert!(
        !line.contains("0.00%"),
        "absent must never render as zero: {line}"
    );
    assert!(
        line.contains("71.90% of all input"),
        "the lifetime figure still shows: {line}"
    );
}

/// Both numbers are labelled for the question each answers — the measured
/// session that started this work reads 71.9% lifetime and ~99% re-read, and
/// showing only the first is what made a healthy cache look broken.
///
/// MUTATION CHECK (executed): drop the lifetime half from the two-number arm.
/// Expected RUNTIME failure: the cold-start assertion below.
#[test]
fn both_numbers_are_shown_and_distinguishable() {
    let line = haider_tui::agent_metrics::cache_line(&usage(Some(7_190), Some(9_900)));
    assert!(
        line.contains("71.90% of all input"),
        "lifetime cold-start cost stays visible: {line}"
    );
    assert!(
        line.contains("99.00% of re-reads"),
        "the health signal is present: {line}"
    );
}

/// No telemetry at all is still honestly nothing.
#[test]
fn no_cache_telemetry_reports_na() {
    let line = haider_tui::agent_metrics::cache_line(&usage(None, Some(9_900)));
    assert_eq!(line, "cache — hit n/a");
}
