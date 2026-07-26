//! Boot-screen content model: the readiness checklist rows, independent of
//! the render stack. Sim parity: `✓` done · `◌` in progress · `·` pending.

use haider_protocol::state::ReadinessCheck;

/// Marker state of one checklist row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckMarker {
    /// Completed (`✓`).
    Done,
    /// The check currently running (`◌`) — the first not-yet-ok check.
    Current,
    /// Not reached yet (`·`).
    Pending,
}

impl CheckMarker {
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Current => "◌",
            Self::Pending => "·",
        }
    }
}

/// One renderable checklist row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckRow<'a> {
    pub marker: CheckMarker,
    pub name: &'a str,
}

impl CheckRow<'_> {
    /// The sim's row text: `✓ store open · journal replayed`.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {}", self.marker.glyph(), self.name)
    }
}

/// Project readiness checks into marker rows: every `ok` check is Done, the
/// first non-ok check is Current, the rest are Pending. (Checks arrive in
/// boot order from `HarnessStatus::Starting`.)
#[must_use]
pub fn check_rows(checks: &[ReadinessCheck]) -> Vec<CheckRow<'_>> {
    let mut current_assigned = false;
    checks
        .iter()
        .map(|check| {
            let marker = if check.ok {
                CheckMarker::Done
            } else if current_assigned {
                CheckMarker::Pending
            } else {
                current_assigned = true;
                CheckMarker::Current
            };
            CheckRow {
                marker,
                name: &check.name,
            }
        })
        .collect()
}

/// The boot sub-line under the wordmark: `v{version} · starting up`.
#[must_use]
pub fn boot_subline(version: &str) -> String {
    format!("v{version} · starting up")
}

/// The launcher sub-line: `v{version} · the lion`.
#[must_use]
pub fn launcher_subline(version: &str) -> String {
    format!("v{version} · the lion")
}
