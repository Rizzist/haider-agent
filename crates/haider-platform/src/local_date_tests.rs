#![allow(clippy::unwrap_used)]
//! Civil-date sampling tests (`docs/design/dated-workspace-v1.md` §1, C5).

use super::{CivilDateSample, civil_from_unix_days, sample_with_offset};

#[test]
fn civil_from_unix_days_pins_known_dates() {
    assert_eq!(civil_from_unix_days(0), (1970, 1, 1));
    assert_eq!(civil_from_unix_days(-1), (1969, 12, 31));
    // 2026-09-13 is Unix day 20_709 (2026-01-01 = 20_454, +255).
    assert_eq!(civil_from_unix_days(20_709), (2026, 9, 13));
    assert_eq!(civil_from_unix_days(19_723), (2024, 1, 1));
    assert_eq!(civil_from_unix_days(19_722), (2023, 12, 31));
}

#[test]
fn offset_shifts_the_civil_date_at_local_midnight() {
    // 2026-09-12T23:30:00Z.
    let utc_ms = (20_708u64 * 86_400 + 23 * 3600 + 30 * 60) * 1000;
    // UTC frame: still the 12th.
    assert_eq!(
        sample_with_offset(utc_ms, 0),
        CivilDateSample {
            year: 2026,
            month: 9,
            day: 12,
            utc_ms,
            offset_seconds: 0,
        }
    );
    // One hour east: past local midnight, the 13th.
    assert_eq!(sample_with_offset(utc_ms, 3600).day, 13);
    // West of UTC stays on the 12th even at 00:30Z the next day.
    let next_ms = (20_709u64 * 86_400 + 30 * 60) * 1000;
    let sample = sample_with_offset(next_ms, -5 * 3600);
    assert_eq!((sample.year, sample.month, sample.day), (2026, 9, 12));
}

#[test]
fn rollover_happens_exactly_at_local_midnight() {
    let offset = 2 * 3600; // two hours east
    // One millisecond before local midnight of 2026-09-13.
    let before_ms = (20_709u64 * 86_400 - u64::try_from(offset).unwrap()) * 1000 - 1;
    let after_ms = before_ms + 1;
    assert_eq!(sample_with_offset(before_ms, offset).day, 12);
    assert_eq!(sample_with_offset(after_ms, offset).day, 13);
}

#[test]
fn live_samples_agree_with_pure_math() {
    let local = super::sample_local_civil_date().unwrap();
    let recomputed = sample_with_offset(local.utc_ms, local.offset_seconds);
    assert_eq!(
        (local.year, local.month, local.day),
        (recomputed.year, recomputed.month, recomputed.day)
    );
    let utc = super::sample_utc_civil_date().unwrap();
    assert_eq!(utc.offset_seconds, 0);
    // Offsets are whole minutes within ±26 h (extreme real zones are ±14 h).
    assert_eq!(local.offset_seconds % 60, 0);
    assert!(local.offset_seconds.abs() <= 26 * 3600);
}
