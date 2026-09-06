//! Shared text formatting for command-line and interactive clients.

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
