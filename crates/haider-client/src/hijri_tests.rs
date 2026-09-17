#![allow(clippy::unwrap_used)]
//! Pinned vectors and structural checks for the islamic-civil conversion
//! (`docs/design/dated-workspace-v1.md` §1).

use super::hijri::{
    HIJRI_CALENDAR_ID, HijriDate, HijriError, hijri_from_epoch_days, hijri_from_gregorian,
    hijri_to_epoch_days, is_hijri_leap_year,
};

fn date(year: i32, month: u8, day: u8) -> HijriDate {
    HijriDate { year, month, day }
}

/// The owner's ratified reference conversion: 2026-09-13 = 1448-03-30.
#[test]
fn pinned_reference_conversion() {
    assert_eq!(
        hijri_from_gregorian(2026, 9, 13).unwrap(),
        date(1448, 3, 30)
    );
}

#[test]
fn pinned_neighbor_conversions() {
    assert_eq!(
        hijri_from_gregorian(2026, 9, 12).unwrap(),
        date(1448, 3, 29)
    );
    assert_eq!(hijri_from_gregorian(2026, 6, 17).unwrap(), date(1448, 1, 1));
}

#[test]
fn epoch_is_first_of_muharram_year_one() {
    assert_eq!(hijri_from_gregorian(622, 7, 19).unwrap(), date(1, 1, 1));
    assert_eq!(hijri_from_epoch_days(0).unwrap(), date(1, 1, 1));
}

/// Day-rollover across the pinned reference: month 3 has 30 days, so the
/// next Gregorian day starts month 4.
#[test]
fn rollover_at_month_boundary() {
    assert_eq!(hijri_from_gregorian(2026, 9, 14).unwrap(), date(1448, 4, 1));
}

/// Consecutive days over two full 30-year cycles stay strictly consecutive
/// and round-trip exactly; also proves the cycle is 10631 days.
#[test]
fn two_full_cycles_are_consecutive_and_round_trip() {
    let mut previous = hijri_from_epoch_days(0).unwrap();
    assert_eq!(hijri_to_epoch_days(previous).unwrap(), 0);
    for day in 1..(2 * 10_631) {
        let current = hijri_from_epoch_days(day).unwrap();
        assert!(
            current > previous,
            "day {day}: {current} did not advance past {previous}"
        );
        let advanced_in_month = current.year == previous.year
            && current.month == previous.month
            && current.day == previous.day + 1;
        let advanced_month = current.year == previous.year
            && current.month == previous.month + 1
            && current.day == 1;
        let advanced_year =
            current.year == previous.year + 1 && current.month == 1 && current.day == 1;
        assert!(
            advanced_in_month || advanced_month || advanced_year,
            "day {day}: non-consecutive step {previous} -> {current}"
        );
        assert_eq!(hijri_to_epoch_days(current).unwrap(), day);
        previous = current;
    }
    assert_eq!(hijri_to_epoch_days(date(31, 1, 1)).unwrap(), 10_631);
}

#[test]
fn leap_cycle_matches_civil_table() {
    let leap: Vec<i64> = (1..=30).filter(|y| is_hijri_leap_year(*y)).collect();
    assert_eq!(leap, vec![2, 5, 7, 10, 13, 16, 18, 21, 24, 26, 29]);
    // The pinned reference year 1448 (cycle year 8) is common.
    assert!(!is_hijri_leap_year(1448));
    assert!(is_hijri_leap_year(1439)); // cycle year 29
}

#[test]
fn month_twelve_length_follows_leap_years() {
    // Common year: month 12 has 29 days.
    assert_eq!(
        hijri_to_epoch_days(date(1, 12, 29)).unwrap() + 1,
        hijri_to_epoch_days(date(2, 1, 1)).unwrap()
    );
    assert_eq!(
        hijri_to_epoch_days(date(1, 12, 30)),
        Err(HijriError::InvalidHijri)
    );
    // Leap year 2: month 12 has 30 days.
    assert_eq!(
        hijri_to_epoch_days(date(2, 12, 30)).unwrap() + 1,
        hijri_to_epoch_days(date(3, 1, 1)).unwrap()
    );
}

#[test]
fn before_epoch_is_an_explicit_error() {
    assert_eq!(
        hijri_from_gregorian(622, 7, 18),
        Err(HijriError::BeforeEpoch)
    );
    assert_eq!(hijri_from_epoch_days(-1), Err(HijriError::BeforeEpoch));
}

#[test]
fn invalid_gregorian_dates_are_rejected() {
    assert_eq!(
        hijri_from_gregorian(2026, 2, 30),
        Err(HijriError::InvalidGregorian)
    );
    assert_eq!(
        hijri_from_gregorian(2026, 13, 1),
        Err(HijriError::InvalidGregorian)
    );
    assert_eq!(
        hijri_from_gregorian(2026, 9, 0),
        Err(HijriError::InvalidGregorian)
    );
    // 2024 was a Gregorian leap year; 2026 is not.
    assert!(hijri_from_gregorian(2024, 2, 29).is_ok());
    assert_eq!(
        hijri_from_gregorian(2026, 2, 29),
        Err(HijriError::InvalidGregorian)
    );
}

#[test]
fn supported_range_ends_after_ah_9999() {
    // AH 9999 (cycle year 9) is common, so its last day is 12-29.
    let last = hijri_to_epoch_days(date(9999, 12, 29)).unwrap();
    assert_eq!(hijri_from_epoch_days(last).unwrap(), date(9999, 12, 29));
    assert_eq!(
        hijri_from_epoch_days(last + 1),
        Err(HijriError::AfterSupportedRange)
    );
    assert_eq!(
        hijri_to_epoch_days(date(10_000, 1, 1)),
        Err(HijriError::AfterSupportedRange)
    );
    assert_eq!(
        hijri_to_epoch_days(date(0, 1, 1)),
        Err(HijriError::BeforeEpoch)
    );
}

#[test]
fn labels_are_ascii_zero_padded() {
    assert_eq!(date(1448, 3, 30).label(), "1448-03-30");
    assert_eq!(date(1, 1, 1).label(), "0001-01-01");
    assert_eq!(date(9999, 12, 29).label(), "9999-12-29");
    assert_eq!(HIJRI_CALENDAR_ID, "islamic-civil");
}
