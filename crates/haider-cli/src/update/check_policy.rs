//! On-open update-check policy and its tiny profile-local rate-limit stamp.

use super::{UpdateAvailability, check_update_availability};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Profile-local state written before an automatic release check begins.
pub(crate) const UPDATE_CHECK_STAMP_FILE: &str = "update-check.timestamp";
pub(crate) const UPDATE_CHECK_INTERVAL_SECS: u64 = 6 * 60 * 60;

static STAMP_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckReservation {
    Due,
    Skipped,
}

/// The deliberately quiet result of an automatic check. Discovery, network,
/// and local policy failures all become `Silent`; a background check must
/// never turn an unavailable release service into a TUI error surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackgroundCheckOutcome {
    Available { version: String },
    Silent,
}

/// True when the command-line twin or `HAIDER_NO_UPDATE_CHECK=1` disables
/// automatic checks. Other environment values are intentionally not truthy.
pub(crate) fn automatic_checks_disabled(flag: bool) -> bool {
    automatic_checks_disabled_with(flag, std::env::var_os("HAIDER_NO_UPDATE_CHECK").as_deref())
}

pub(crate) fn automatic_checks_disabled_with(flag: bool, env_value: Option<&OsStr>) -> bool {
    flag || env_value == Some(OsStr::new("1"))
}

/// Pure rate-limit decision over an injected clock. A future stamp is treated
/// as recent, avoiding a check storm after a backwards wall-clock adjustment.
#[must_use]
pub(crate) fn check_due(last_checked: Option<u64>, now_unix_seconds: u64) -> bool {
    match last_checked {
        None => true,
        Some(last) if now_unix_seconds < last => false,
        Some(last) => now_unix_seconds - last >= UPDATE_CHECK_INTERVAL_SECS,
    }
}

#[must_use]
pub(crate) fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Atomically reserves an automatic check and records its start time. The
/// caller supplies the clock so the six-hour law is testable without sleep.
/// `bypass_rate_limit` is for an explicit `/update`; it still refreshes the
/// stamp so a later on-open check does not immediately repeat the request.
pub(crate) fn reserve_check(
    profile_dir: &Path,
    now_unix_seconds: u64,
    bypass_rate_limit: bool,
) -> io::Result<CheckReservation> {
    let stamp = profile_dir.join(UPDATE_CHECK_STAMP_FILE);
    let last_checked = read_stamp(&stamp)?;
    if !bypass_rate_limit && !check_due(last_checked, now_unix_seconds) {
        return Ok(CheckReservation::Skipped);
    }
    write_stamp_atomic(profile_dir, &stamp, now_unix_seconds)?;
    Ok(CheckReservation::Due)
}

/// Runs release discovery with `haider update --check` semantics but collapses
/// every non-actionable result to silence. In particular, equal/older
/// releases and offline GitHub discovery never surface as startup errors.
pub(crate) fn background_check() -> BackgroundCheckOutcome {
    background_outcome_from_discovery(check_update_availability())
}

pub(crate) fn background_outcome_from_discovery(
    outcome: Result<UpdateAvailability, super::UpdateError>,
) -> BackgroundCheckOutcome {
    match outcome {
        Ok(UpdateAvailability::Available { latest, .. }) => {
            BackgroundCheckOutcome::Available { version: latest }
        }
        Ok(UpdateAvailability::Current { .. }) | Err(_) => BackgroundCheckOutcome::Silent,
    }
}

fn read_stamp(path: &Path) -> io::Result<Option<u64>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(raw.trim().parse::<u64>().ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_stamp_atomic(profile_dir: &Path, stamp: &Path, now_unix_seconds: u64) -> io::Result<()> {
    let nonce = STAMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let temporary = profile_dir.join(format!(
        ".{UPDATE_CHECK_STAMP_FILE}.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        writeln!(file, "{now_unix_seconds}")?;
        file.sync_all()?;
        drop(file);
        haider_platform::replace_file(&temporary, stamp)?;
        haider_platform::sync_directory(profile_dir)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
