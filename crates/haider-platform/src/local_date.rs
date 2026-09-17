//! Civil-date sampling for dated workspace allocation
//! (`docs/design/dated-workspace-v1.md` §1, C5).
//!
//! "Today" is the host's civil date at the sampling instant under the OS
//! local offset (or UTC when the caller's policy says so). The sample also
//! captures the UTC instant and the observed offset so an allocation's
//! original date can be audited later without re-deriving it from a
//! different timezone. Locale never participates; the calendar conversion
//! itself lives in the client crate and is pure.

#[cfg(test)]
#[path = "local_date_tests.rs"]
mod tests;

use std::time::{SystemTime, UNIX_EPOCH};

/// One sampled civil date plus the facts needed to audit it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDateSample {
    /// Proleptic-Gregorian civil year in the sampled frame.
    pub year: i32,
    /// Month 1–12.
    pub month: u8,
    /// Day 1–31.
    pub day: u8,
    /// The UTC sampling instant, Unix milliseconds.
    pub utc_ms: u64,
    /// Offset applied to UTC for this sample, seconds east (0 for UTC).
    pub offset_seconds: i32,
}

/// Failures are explicit: the caller must choose UTC or abort, never fall
/// back to a silently different date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CivilDateError {
    /// The system clock reports a time before the Unix epoch.
    ClockBeforeEpoch,
    /// The OS local offset could not be resolved.
    OffsetUnavailable,
}

impl std::fmt::Display for CivilDateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::ClockBeforeEpoch => "system clock is before the Unix epoch",
            Self::OffsetUnavailable => "local UTC offset is unavailable",
        };
        f.write_str(text)
    }
}

impl std::error::Error for CivilDateError {}

/// Samples the current civil date in the host's local timezone.
pub fn sample_local_civil_date() -> Result<CivilDateSample, CivilDateError> {
    let utc_ms = unix_millis_now()?;
    let offset_seconds = local_offset_seconds(utc_ms / 1000)?;
    Ok(sample_with_offset(utc_ms, offset_seconds))
}

/// Samples the current civil date in UTC (offset 0). This is the explicit
/// fallback the caller may choose when the local offset is unavailable.
pub fn sample_utc_civil_date() -> Result<CivilDateSample, CivilDateError> {
    let utc_ms = unix_millis_now()?;
    Ok(sample_with_offset(utc_ms, 0))
}

fn unix_millis_now() -> Result<u64, CivilDateError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CivilDateError::ClockBeforeEpoch)?;
    u64::try_from(elapsed.as_millis()).map_err(|_| CivilDateError::ClockBeforeEpoch)
}

/// Applies an offset to a UTC instant and reads the civil date of the
/// shifted frame. Pure; exercised directly by the tests.
#[must_use]
pub fn sample_with_offset(utc_ms: u64, offset_seconds: i32) -> CivilDateSample {
    let shifted_seconds = i64::try_from(utc_ms / 1000)
        .unwrap_or(i64::MAX)
        .saturating_add(i64::from(offset_seconds));
    let days = shifted_seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_unix_days(days);
    CivilDateSample {
        year,
        month,
        day,
        utc_ms,
        offset_seconds,
    }
}

/// Proleptic-Gregorian civil date from days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`, integer-only).
#[must_use]
pub fn civil_from_unix_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        i32::try_from(year).unwrap_or(i32::MAX),
        u8::try_from(month).unwrap_or(0),
        u8::try_from(day).unwrap_or(0),
    )
}

/// The OS local offset (seconds east of UTC) for the given Unix time.
#[cfg(unix)]
#[allow(unsafe_code)]
fn local_offset_seconds(unix_seconds: u64) -> Result<i32, CivilDateError> {
    let time =
        libc::time_t::try_from(unix_seconds).map_err(|_| CivilDateError::ClockBeforeEpoch)?;
    // SAFETY: `localtime_r` writes only into the provided `tm` out-pointer
    // and reads only the provided time value; both live on this stack frame.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&time, &mut tm) };
    if result.is_null() {
        return Err(CivilDateError::OffsetUnavailable);
    }
    i32::try_from(tm.tm_gmtoff).map_err(|_| CivilDateError::OffsetUnavailable)
}

/// Windows derives the offset from the difference between the OS local and
/// system (UTC) wall clocks, which reflects the active DST rule without
/// interpreting timezone records here.
#[cfg(windows)]
#[allow(unsafe_code)]
fn local_offset_seconds(_unix_seconds: u64) -> Result<i32, CivilDateError> {
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    // SAFETY: both calls only write into the provided SYSTEMTIME out
    // structures on this stack frame and cannot fail.
    let (local, system) = unsafe {
        let mut local = std::mem::zeroed();
        let mut system = std::mem::zeroed();
        GetLocalTime(&mut local);
        GetSystemTime(&mut system);
        (local, system)
    };
    let local_seconds = systemtime_to_epoch_seconds(&local)?;
    let system_seconds = systemtime_to_epoch_seconds(&system)?;
    let offset = local_seconds - system_seconds;
    // Round to the nearest minute: the two clock reads are instants apart.
    let offset = (offset + if offset >= 0 { 30 } else { -30 }) / 60 * 60;
    i32::try_from(offset).map_err(|_| CivilDateError::OffsetUnavailable)
}

#[cfg(windows)]
fn systemtime_to_epoch_seconds(
    time: &windows_sys::Win32::Foundation::SYSTEMTIME,
) -> Result<i64, CivilDateError> {
    let year = i64::from(time.wYear);
    let month = i64::from(time.wMonth);
    if !(1..=12).contains(&month) {
        return Err(CivilDateError::OffsetUnavailable);
    }
    // Hinnant's `days_from_civil`, integer-only.
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = adjusted_year.div_euclid(400);
    let yoe = adjusted_year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(time.wDay) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400
        + i64::from(time.wHour) * 3600
        + i64::from(time.wMinute) * 60
        + i64::from(time.wSecond))
}
