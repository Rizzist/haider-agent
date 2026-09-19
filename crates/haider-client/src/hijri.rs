//! Deterministic tabular `islamic-civil` calendar conversion.
//!
//! Contract: `docs/design/dated-workspace-v1.md` §1. The civil (Friday)
//! epoch — 1 Muharram 1 AH = proleptic Gregorian 0622-07-19 = Julian day
//! number 1948440 — with the fixed 30-year leap cycle. Pure checked integer
//! arithmetic: no floating point, no locale, no network, no OS calendar.
//!
//! This module converts dates; sampling "today" (local-midnight semantics,
//! offset capture) lives in `haider-platform`.

/// Unicode calendar identifier recorded beside every dated allocation so a
/// folder label is never mistaken for an observation-based religious date.
pub const HIJRI_CALENDAR_ID: &str = "islamic-civil";

/// Julian day number of 1 Muharram 1 AH in the civil (Friday) epoch.
const CIVIL_EPOCH_JDN: i64 = 1_948_440;

/// Supported AH year range; outside is an explicit error, never a wrap.
const MIN_AH_YEAR: i64 = 1;
const MAX_AH_YEAR: i64 = 9999;

/// Days in one complete 30-year tabular cycle (19*354 + 11*355).
const CYCLE_DAYS: i64 = 10_631;

/// A converted islamic-civil calendar date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HijriDate {
    /// AH year, 1–9999.
    pub year: i32,
    /// Month 1–12.
    pub month: u8,
    /// Day 1–30.
    pub day: u8,
}

impl HijriDate {
    /// ASCII `YYYY-MM-DD` directory label: zero-padded month/day, year
    /// padded to at least four digits.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl std::fmt::Display for HijriDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Conversion failures. Every variant is an explicit refusal; the module
/// never clamps or wraps a date it cannot represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HijriError {
    /// The Gregorian input is not a real proleptic-Gregorian calendar date.
    InvalidGregorian,
    /// The Hijri input is not a real islamic-civil calendar date.
    InvalidHijri,
    /// The date is before 1 Muharram 1 AH.
    BeforeEpoch,
    /// The date falls after AH 9999.
    AfterSupportedRange,
    /// Checked integer arithmetic overflowed (absurd input years).
    Overflow,
}

impl std::fmt::Display for HijriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::InvalidGregorian => "not a valid Gregorian calendar date",
            Self::InvalidHijri => "not a valid islamic-civil calendar date",
            Self::BeforeEpoch => "date is before 1 Muharram 1 AH (0622-07-19)",
            Self::AfterSupportedRange => "date is after supported AH year 9999",
            Self::Overflow => "calendar arithmetic overflow",
        };
        f.write_str(text)
    }
}

impl std::error::Error for HijriError {}

/// Whether the AH year is a leap year of the fixed civil 30-year cycle
/// {2, 5, 7, 10, 13, 16, 18, 21, 24, 26, 29}.
/// Cycle year 30 (`year % 30 == 0`) is NOT a leap year in the civil table:
/// the eleventh leap day of each cycle accrues in cycle year 29.
#[must_use]
pub fn is_hijri_leap_year(year: i64) -> bool {
    matches!(
        year.rem_euclid(30),
        2 | 5 | 7 | 10 | 13 | 16 | 18 | 21 | 24 | 26 | 29
    )
}

/// Days of the AH year before month `month` (1-based) begins.
fn days_before_month(month: u8) -> i64 {
    // Months alternate 30/29 starting with 30; the month-12 leap day is
    // AFTER the last month starts, so it never affects a start offset.
    let m = i64::from(month) - 1;
    30 * ((m + 1) / 2) + 29 * (m / 2)
}

/// Length in days of `month` in AH year `year`.
fn hijri_month_length(year: i64, month: u8) -> i64 {
    if month == 12 {
        if is_hijri_leap_year(year) { 30 } else { 29 }
    } else if month % 2 == 1 {
        30
    } else {
        29
    }
}

/// Days from the civil epoch to 1 Muharram of AH `year`:
/// `354*(y-1) + floor((3 + 11*y)/30)`.
fn hijri_year_start(year: i64) -> Result<i64, HijriError> {
    let base = 354i64.checked_mul(year - 1).ok_or(HijriError::Overflow)?;
    let leap = 11i64
        .checked_mul(year)
        .and_then(|v| v.checked_add(3))
        .ok_or(HijriError::Overflow)?
        .div_euclid(30);
    base.checked_add(leap).ok_or(HijriError::Overflow)
}

/// Julian day number of a proleptic-Gregorian date, validated first.
fn gregorian_to_jdn(year: i32, month: u8, day: u8) -> Result<i64, HijriError> {
    if !(1..=12).contains(&month)
        || day == 0
        || i64::from(day) > gregorian_month_length(year, month)
    {
        return Err(HijriError::InvalidGregorian);
    }
    let a = i64::from(14 - i32::from(month)) / 12;
    let y = i64::from(year)
        .checked_add(4800 - a)
        .ok_or(HijriError::Overflow)?;
    let m = i64::from(month) + 12 * a - 3;
    let term_month = (153 * m + 2) / 5;
    365i64
        .checked_mul(y)
        .and_then(|v| v.checked_add(y.div_euclid(4)))
        .and_then(|v| v.checked_sub(y.div_euclid(100)))
        .and_then(|v| v.checked_add(y.div_euclid(400)))
        .and_then(|v| v.checked_add(term_month))
        .and_then(|v| v.checked_add(i64::from(day)))
        .and_then(|v| v.checked_sub(32045))
        .ok_or(HijriError::Overflow)
}

fn gregorian_month_length(year: i32, month: u8) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

/// Converts a proleptic-Gregorian civil date to the islamic-civil date.
pub fn hijri_from_gregorian(year: i32, month: u8, day: u8) -> Result<HijriDate, HijriError> {
    let jdn = gregorian_to_jdn(year, month, day)?;
    let days = jdn
        .checked_sub(CIVIL_EPOCH_JDN)
        .ok_or(HijriError::Overflow)?;
    hijri_from_epoch_days(days)
}

/// Converts a 0-based day count since the civil epoch (day 0 = 1 Muharram
/// 1 AH) to the islamic-civil date.
pub fn hijri_from_epoch_days(days: i64) -> Result<HijriDate, HijriError> {
    if days < 0 {
        return Err(HijriError::BeforeEpoch);
    }
    // Bounded search seeded by the exact mean cycle length, then corrected
    // by at most a couple of steps; validated by the exhaustive rollover
    // tests over full cycles.
    let mut year = days
        .checked_mul(30)
        .ok_or(HijriError::Overflow)?
        .div_euclid(CYCLE_DAYS)
        + 1;
    if year < MIN_AH_YEAR {
        year = MIN_AH_YEAR;
    }
    while year > MIN_AH_YEAR && hijri_year_start(year)? > days {
        year -= 1;
    }
    while hijri_year_start(year + 1)? <= days {
        year += 1;
    }
    if year > MAX_AH_YEAR {
        return Err(HijriError::AfterSupportedRange);
    }
    let mut remaining = days - hijri_year_start(year)?;
    let mut month: u8 = 1;
    while month <= 12 {
        let length = hijri_month_length(year, month);
        if remaining < length {
            let converted_year = i32::try_from(year).map_err(|_| HijriError::Overflow)?;
            let converted_day = u8::try_from(remaining + 1).map_err(|_| HijriError::Overflow)?;
            return Ok(HijriDate {
                year: converted_year,
                month,
                day: converted_day,
            });
        }
        remaining -= length;
        month += 1;
    }
    // Unreachable when year-start offsets and month lengths agree; refuse
    // rather than fabricate a date if they ever diverge.
    Err(HijriError::Overflow)
}

/// Inverse conversion (islamic-civil date to 0-based epoch days), used by
/// the round-trip tests and range validation.
pub fn hijri_to_epoch_days(date: HijriDate) -> Result<i64, HijriError> {
    let year = i64::from(date.year);
    if !(MIN_AH_YEAR..=MAX_AH_YEAR).contains(&year) {
        return Err(if year < MIN_AH_YEAR {
            HijriError::BeforeEpoch
        } else {
            HijriError::AfterSupportedRange
        });
    }
    if !(1..=12).contains(&date.month)
        || date.day == 0
        || i64::from(date.day) > hijri_month_length(year, date.month)
    {
        return Err(HijriError::InvalidHijri);
    }
    Ok(hijri_year_start(year)? + days_before_month(date.month) + i64::from(date.day) - 1)
}
