//! Reset-instant presentation for `/usage` (973 usage UI).
//!
//! Provider meters publish reset instants as Unix milliseconds. The owner
//! reads them in their own time zone, so every instant renders in LOCAL time
//! with the exact UTC beside it, plus a relative delta. Three rules hold:
//!
//! - The zone is resolved PER INSTANT ([`UsageTimeZone::offset_at`]), so a
//!   daylight-saving change between the snapshot and a reset is honoured.
//! - Relative deltas are measured from the report's own `generated_at_ms`
//!   (the snapshot), never the client clock — a frame stays a pure function
//!   of the snapshot, and minutes FLOOR (a reset never claims more runway
//!   than it has).
//! - An unresolvable local zone is said out loud: the instant renders in UTC
//!   only, never a silent UTC labelled "local".

/// Which frame `/usage` renders local times in.
#[derive(Debug, Clone, Copy, Default)]
pub enum UsageTimeZone {
    /// No local zone: instants render in UTC only. The deterministic default
    /// (tests and headless paths never read the host zone by accident).
    #[default]
    UtcOnly,
    /// The host OS zone, resolved for each instant (the live TUI).
    Host,
    /// A fixed offset in seconds east of UTC.
    Fixed(i32),
    /// A per-instant rule: seconds east of UTC for a Unix second, `None`
    /// when unresolvable. Lets tests pin a real DST transition.
    Rule(fn(i64) -> Option<i32>),
}

impl UsageTimeZone {
    /// Seconds east of UTC in effect at `unix_seconds`; `None` when this
    /// frame has no local zone or the host cannot resolve one.
    #[must_use]
    pub fn offset_at(self, unix_seconds: i64) -> Option<i32> {
        match self {
            Self::UtcOnly => None,
            Self::Host => u64::try_from(unix_seconds)
                .ok()
                .and_then(|seconds| haider_platform::local_utc_offset_seconds(seconds).ok()),
            Self::Fixed(offset) => Some(offset),
            Self::Rule(rule) => rule(unix_seconds),
        }
    }

    /// Offset in effect at a millisecond instant.
    #[must_use]
    pub fn offset_at_ms(self, timestamp_ms: u64) -> Option<i32> {
        self.offset_at(i64::try_from(timestamp_ms / 1_000).unwrap_or(i64::MAX))
    }
}

/// One instant broken down in a given frame (UTC or a local offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilInstant {
    /// Days since 1970-01-01 in this frame (the calendar grid's day key).
    pub day: i64,
    pub year: i64,
    pub month: u32,
    pub date: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Break `timestamp_ms` down after shifting it `offset_seconds` east.
#[must_use]
pub fn civil_instant(timestamp_ms: u64, offset_seconds: i32) -> CivilInstant {
    let seconds = i64::try_from(timestamp_ms / 1_000)
        .unwrap_or(i64::MAX)
        .saturating_add(i64::from(offset_seconds));
    let day = seconds.div_euclid(86_400);
    let in_day = seconds.rem_euclid(86_400);
    let (year, month, date) = crate::format::civil_from_days(day);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    CivilInstant {
        day,
        year,
        month,
        date,
        weekday: crate::format::weekday_from_days(day),
        hour: (in_day / 3_600) as u32,
        minute: ((in_day % 3_600) / 60) as u32,
    }
}

/// The calendar day an instant falls on in `zone` (UTC when no local zone).
#[must_use]
pub fn zoned_day(timestamp_ms: u64, zone: UsageTimeZone) -> i64 {
    civil_instant(timestamp_ms, zone.offset_at_ms(timestamp_ms).unwrap_or(0)).day
}

/// `UTC+03:30` / `UTC-04:00` / `UTC+00:00`.
#[must_use]
pub fn fmt_utc_offset(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let magnitude = offset_seconds.unsigned_abs();
    format!(
        "UTC{sign}{:02}:{:02}",
        magnitude / 3_600,
        (magnitude % 3_600) / 60
    )
}

/// `Fri 25 Sep`, with the year appended when it differs from
/// `reference_year` (`Fri 01 Jan 2027`).
#[must_use]
pub fn fmt_civil_date(instant: &CivilInstant, reference_year: i64) -> String {
    let weekday = WEEKDAYS[instant.weekday as usize % 7];
    let month = instant
        .month
        .checked_sub(1)
        .and_then(|month| MONTHS.get(month as usize))
        .copied()
        .unwrap_or("???");
    if instant.year == reference_year {
        format!("{weekday} {:02} {month}", instant.date)
    } else {
        format!("{weekday} {:02} {month} {:04}", instant.date, instant.year)
    }
}

/// `HH:MM`.
#[must_use]
pub fn fmt_clock(instant: &CivilInstant) -> String {
    format!("{:02}:{:02}", instant.hour, instant.minute)
}

/// The owner-facing reset instant:
///
/// - local zone known, non-zero offset: `Fri 25 Sep · 12:59 local (16:59 UTC)`
///   — and when the UTC date differs, it is named too:
///   `Fri 25 Sep · 23:30 local (Sat 26 Sep 03:30 UTC)`;
/// - local zone known, zero offset: `Fri 25 Sep · 16:59 local = UTC`;
/// - no local zone: `Fri 25 Sep · 16:59 UTC`.
#[must_use]
pub fn fmt_reset_instant(timestamp_ms: u64, zone: UsageTimeZone, reference_year: i64) -> String {
    let utc = civil_instant(timestamp_ms, 0);
    match zone.offset_at_ms(timestamp_ms) {
        None => format!(
            "{} · {} UTC",
            fmt_civil_date(&utc, reference_year),
            fmt_clock(&utc)
        ),
        Some(0) => format!(
            "{} · {} local = UTC",
            fmt_civil_date(&utc, reference_year),
            fmt_clock(&utc)
        ),
        Some(offset) => {
            let local = civil_instant(timestamp_ms, offset);
            let utc_part = if utc.day == local.day {
                format!("{} UTC", fmt_clock(&utc))
            } else {
                format!(
                    "{} {} UTC",
                    fmt_civil_date(&utc, reference_year),
                    fmt_clock(&utc)
                )
            };
            format!(
                "{} · {} local ({utc_part})",
                fmt_civil_date(&local, reference_year),
                fmt_clock(&local)
            )
        }
    }
}

/// The compact form for one-line summaries: `Thu 24 Sep 00:39 local`, or
/// `Thu 24 Sep 00:39 UTC` without a local zone (the list rows carry both).
#[must_use]
pub fn fmt_reset_short(timestamp_ms: u64, zone: UsageTimeZone, reference_year: i64) -> String {
    match zone.offset_at_ms(timestamp_ms) {
        None => {
            let utc = civil_instant(timestamp_ms, 0);
            format!(
                "{} {} UTC",
                fmt_civil_date(&utc, reference_year),
                fmt_clock(&utc)
            )
        }
        Some(offset) => {
            let local = civil_instant(timestamp_ms, offset);
            format!(
                "{} {} local",
                fmt_civil_date(&local, reference_year),
                fmt_clock(&local)
            )
        }
    }
}

/// Delta from the snapshot to an instant: `in 1d 3h`, `in 3h 2m`, `in 12m`,
/// `due now` (under a minute either way), `2h 5m ago` (already passed at
/// snapshot time). Minutes floor in both directions.
#[must_use]
pub fn fmt_relative(snapshot_ms: u64, timestamp_ms: u64) -> String {
    let (delta_ms, future) = if timestamp_ms >= snapshot_ms {
        (timestamp_ms - snapshot_ms, true)
    } else {
        (snapshot_ms - timestamp_ms, false)
    };
    let minutes = delta_ms / 60_000;
    if minutes == 0 {
        return "due now".to_owned();
    }
    let days = minutes / (24 * 60);
    let hours = (minutes % (24 * 60)) / 60;
    let mins = minutes % 60;
    let span = if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    };
    if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

/// The calendar's zone caption: `local UTC-04:00` or `UTC (local zone
/// unavailable)`. The offset is the one in effect at the snapshot; a DST
/// change later in the month is still honoured per instant.
#[must_use]
pub fn fmt_zone_caption(snapshot_ms: u64, zone: UsageTimeZone) -> String {
    match (zone, zone.offset_at_ms(snapshot_ms)) {
        (_, Some(offset)) => format!("local {}", fmt_utc_offset(offset)),
        (UsageTimeZone::UtcOnly, None) => "UTC".to_owned(),
        (_, None) => "UTC (local zone unavailable)".to_owned(),
    }
}
