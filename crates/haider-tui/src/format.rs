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

/// The `/usage` limit bars' width in cells (U2).
pub const USAGE_BAR_CELLS: usize = 10;

/// A `/usage` limit bar in REMAINING semantics: the wire's 0.0–1.0
/// utilization fraction rendered as what is LEFT — the bar drains as the
/// plan depletes (`▰▰▱▱▱▱▱▱▱▱` for a window 83% consumed). Owner call
/// (2026-08-24): a plan is read as a budget, so the eye tracks runway
/// going down, not consumption going up. The wire value stays verbatim
/// utilization; the flip happens only here at the display layer.
///
/// BAR-MATH LAW, mirrored from the used-semantics original: input clamps
/// to [0, 1], fill is FLOOR-based on the REMAINING fraction — never
/// rounding a nearly-spent window up to more runway — with two honesty
/// clamps: any nonzero remaining shows at least one filled cell, and any
/// nonzero consumption keeps at least one empty cell. Only a genuinely
/// untouched window (0.0 used) renders full; only a genuinely exhausted
/// one (≥ 1.0 used) renders empty.
#[must_use]
pub fn remaining_bar(utilization: f64, cells: usize) -> String {
    let remaining = 1.0 - utilization.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut full = ((remaining * cells as f64).floor() as usize).min(cells);
    if remaining > 0.0 && full == 0 {
        full = 1;
    }
    if remaining < 1.0 && full == cells {
        full = cells.saturating_sub(1);
    }
    let mut out = String::with_capacity(cells * "▰".len());
    for _ in 0..full {
        out.push('▰');
    }
    for _ in full..cells {
        out.push('▱');
    }
    out
}

/// The `/usage` label beside a remaining bar: the runway as a whole
/// percent with the word that names the semantics (`0.83` used →
/// `17% left`). Remaining FLOORS where the old used-label rounded —
/// the same never-overstate-runway rule `fmt_reset` applies to time.
#[must_use]
pub fn fmt_remaining(utilization: f64) -> String {
    let remaining = 1.0 - utilization.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = (remaining * 100.0).floor() as u32;
    format!("{pct}% left")
}

/// Reset-instant formatting for `/usage` windows (U2): the delta from the
/// report's own `generated_at_ms` to `resets_at_ms`, BOTH on the daemon's
/// clock — never the client's, so the line is a pure function of the
/// snapshot.
///
/// RESET-TIME LAW, tiered like [`fmt_elapsed`]: an elapsed or sub-minute
/// reset says `resets soon`; under an hour `resets in {m}m`; under a day
/// `resets in {h}h {m}m`; from a day up `resets in {d}d {h}h`. Minutes
/// floor (a window never claims more runway than it has).
#[must_use]
pub fn fmt_reset(generated_at_ms: u64, resets_at_ms: u64) -> String {
    let delta_ms = resets_at_ms.saturating_sub(generated_at_ms);
    let minutes = delta_ms / 60_000;
    if minutes == 0 {
        return "resets soon".to_owned();
    }
    let days = minutes / (24 * 60);
    let hours = (minutes % (24 * 60)) / 60;
    let mins = minutes % 60;
    if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {mins}m")
    } else {
        format!("resets in {mins}m")
    }
}

/// Formats short provider backoffs with second precision. Sub-second or
/// elapsed delays say `resets soon`; unlike [`fmt_reset`], this must not
/// floor short delays to minutes.
#[must_use]
pub fn fmt_reset_in(delta_ms: u64) -> String {
    if delta_ms < 1000 {
        "resets soon".to_owned()
    } else {
        format!("resets in {}", fmt_elapsed(delta_ms))
    }
}

/// The ink a `/usage` bar's filled cells wear, from the same 0.0–1.0
/// fraction the bar renders (U2). THRESHOLD LAW: below 0.70 the calm `ok`
/// slot, from 0.70 the `warn` slot, from 0.90 the `err` slot — theme
/// slots only, never raw colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageTone {
    Ok,
    Warn,
    Err,
}

/// Streamer-friendly identity masking (U2 owner addendum; P1 extended it
/// to every surface that renders an account identity). THE ONE AUTHORITY
/// — no second mask dialect exists: `/usage` identity lines, `/accounts`
/// rows, import/OAuth receipts, and the login card's committed identity
/// all pass through here.
///
/// MASK LAW: emails keep the first character of the local part and the
/// first character of the domain, up to eight `*` for the remaining
/// characters (so the secret's exact length is not disclosed), with the
/// final `.tld` left readable —
/// `support@diffforge.ai` → `s******@d********.ai`. Non-email identities
/// mask the same way as one part. The masked form never contains the
/// full local part.
///
/// NOT masked anywhere, by design: account ALIASES — the daemon's alias
/// grammar (`[a-z0-9][a-z0-9._-]{0,63}`, no `@`) means an alias can never
/// be an email, and U2 shipped `/usage`'s alias chips unmasked. The
/// launcher header's `account <alias>` segment and `/providers`' active-
/// account line render aliases, so they carry no mask (a masked alias
/// would be a second dialect, not more safety).
#[must_use]
pub fn mask_identity(identity: &str) -> String {
    const MAX_MASKED_RUN: usize = 8;

    fn mask_part(part: &str) -> String {
        let mut chars = part.chars();
        chars.next().map_or_else(String::new, |first| {
            let rest = chars.count().min(MAX_MASKED_RUN);
            let mut out = String::with_capacity(part.len());
            out.push(first);
            out.push_str(&"*".repeat(rest));
            out
        })
    }
    match identity.split_once('@') {
        Some((local, domain)) => {
            let masked_domain = domain.rsplit_once('.').map_or_else(
                || mask_part(domain),
                |(name, tld)| format!("{}.{tld}", mask_part(name)),
            );
            format!("{}@{masked_domain}", mask_part(local))
        }
        None => mask_part(identity),
    }
}

/// See [`UsageTone`].
#[must_use]
pub fn usage_tone(utilization: f64) -> UsageTone {
    let clamped = utilization.clamp(0.0, 1.0);
    if clamped >= 0.90 {
        UsageTone::Err
    } else if clamped >= 0.70 {
        UsageTone::Warn
    } else {
        UsageTone::Ok
    }
}
