//! Display formatters shared across screens.
//!
//! Parity source: the `/tui` sim's `fmtTok` and `meterCells`. Ports are
//! integer-exact where the sim's float formatting is well-defined, including
//! its quirks (`9000 → "9.0k"` keeps the trailing decimal; only the `M` tier
//! strips a trailing `.0`).

/// Token counts for the status bar and session metadata: `842` · `9.0k` ·
/// `131k` · `1.5M` · `2M`.
///
/// Deliberate deviation from JS `toFixed` at exact half boundaries: this is
/// mathematical half-up (1150 → `1.2k`), while the sim's float
/// representation yields `1.1k` (1.15 stored as 1.1499…). Predictable
/// integer rounding wins over emulating float artifacts.
#[must_use]
pub fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        // One decimal in units of M, trailing .0 stripped (sim: 2M, 1.5M).
        // Round-half-up via remainder compare — `n + 50_000` would overflow
        // near u64::MAX (efficiency rider #9).
        let tenths = n / 100_000 + u64::from(n % 100_000 >= 50_000);
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    } else if n >= 10_000 {
        format!("{}k", (n + 500) / 1000)
    } else if n >= 1000 {
        // Sim keeps the decimal below 10k, including x.0 (toFixed(1)).
        let tenths = (n + 50) / 100;
        format!("{}.{}k", tenths / 10, tenths % 10)
    } else {
        n.to_string()
    }
}

/// Durations for the subtree rows (S4), law-pinned h/m/s: `42s` below a
/// minute, `25m 18s` below an hour, `1h 4m 9s` from an hour up — units
/// descend, no zero-padding, and every lower unit present once its tier is
/// reached (`1h 0m 42s`, never `1h 42s`). Seconds truncate (a second shows
/// only once it has fully elapsed), so the live tick can never overshoot a
/// later frozen final.
#[must_use]
pub fn fmt_elapsed(ms: u64) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// The context meter: `pct` of `cells` rendered as `▰▰▰▱▱▱▱▱▱▱`.
/// The sim clamps only the top; we also clamp negatives (unreachable from
/// valid ratios) instead of panicking.
#[must_use]
pub fn meter_cells(pct: f64, cells: usize) -> String {
    let clamped = pct.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let full = ((clamped * cells as f64).round() as usize).min(cells);
    let mut out = String::with_capacity(cells * "▰".len());
    for _ in 0..full {
        out.push('▰');
    }
    for _ in full..cells {
        out.push('▱');
    }
    out
}

/// The status-bar meter width used by the sim (`meterCells(pct)`).
pub const METER_CELLS_DEFAULT: usize = 10;
