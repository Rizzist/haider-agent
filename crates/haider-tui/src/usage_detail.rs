//! `/usage` calendar — the selected account's DETAIL panel and the
//! owner-facing meter-failure wording (973 usage UI).
//!
//! The panel is shaped for data the provider may or may not report: the
//! Haider Code weekly allowance, previous resets, banked / used / remaining
//! resets, and a "use banked reset" action. Every field is optional and
//! absence is SAID ("not reported by provider") — nothing is inferred, and
//! no production path fabricates a value.
//!
//! Wire status (973): the usage report carries none of the banked-reset or
//! reset-history fields yet, so [`account_extras`] fills only what the
//! current wire can prove (a Haider Code account's published `weekly`
//! window). The typed [`AccountResetExtras`] is the landing surface for the
//! backend follow-up: mapping new report fields into it covers the DISPLAY.
//! Enabling the action additionally needs a request path: `b` today only
//! explains unavailable states, and an `Available` action must send the
//! daemon's banked-reset request (which owns the always-ask approval) and
//! render its pending/approved/denied result.

use haider_protocol::usage::{AccountMeterStateV1, AccountUsageReportV1, UsageWindowV1};

use crate::usage_time::{UsageTimeZone, fmt_relative, fmt_reset_instant};

/// Provider id of Haider Code's own accounts.
pub const HAIDER_CODE_PROVIDER: &str = "haider-code";

/// The Haider Code weekly allowance as published (a real meter window).
#[derive(Debug, Clone, PartialEq)]
pub struct AllowanceView {
    /// Provider-native window name the value came from.
    pub window: String,
    /// Fraction used, 0.0–1.0, when the provider stated one.
    pub utilization: Option<f64>,
    pub resets_at_ms: Option<u64>,
}

/// Where a previous-reset record came from. The device journal is labelled
/// as such: it is this machine's observation, not provider truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetHistorySource {
    Provider,
    DeviceJournal,
}

/// One previous reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastReset {
    /// Window name (`five_hour`, `seven_day`, `5h`, `weekly`, ...).
    pub window: String,
    pub at_ms: u64,
    pub source: ResetHistorySource,
}

/// Banked-reset counters, each only when the provider states it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BankedResets {
    pub banked: Option<u32>,
    pub used: Option<u32>,
    pub remaining: Option<u32>,
}

/// Whether "use banked reset" can be offered for this account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BankedResetAction {
    /// Nothing on the wire says whether an operation exists (the 973 usage
    /// report carries no such field). Unknown is never inferred: this is
    /// "not reported", NOT "not offered". Disabled.
    #[default]
    NotReported,
    /// The provider/daemon explicitly reported that no operation exists
    /// (a future wire state). Disabled.
    NotOffered,
    /// The provider operation exists but cannot be used now (typed reason,
    /// e.g. `no banked resets remaining`): disabled with that reason.
    Unavailable { reason: String },
    /// Usable. Triggering it always goes through an always-ask approval.
    Available,
}

/// Everything the detail panel can show beyond the meter windows.
/// `None` means "the provider did not report this" — rendered as such.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountResetExtras {
    /// Haider Code accounts only.
    pub allowance: Option<AllowanceView>,
    /// `Some(vec![])` is a REPORTED empty history, distinct from `None`.
    pub history: Option<Vec<PastReset>>,
    pub banked: Option<BankedResets>,
    pub action: BankedResetAction,
}

/// The extras the CURRENT wire can prove for `account`. Only a Haider Code
/// account's published, unlabeled `weekly`/`seven_day` window becomes an
/// allowance; history, banked counters and the action stay "not reported"
/// until the usage report carries them.
#[must_use]
pub fn account_extras(account: &AccountUsageReportV1) -> AccountResetExtras {
    AccountResetExtras {
        allowance: haider_code_weekly_window(account).map(|window| AllowanceView {
            window: window.window.clone(),
            utilization: Some(window.utilization),
            resets_at_ms: window.resets_at_ms,
        }),
        ..AccountResetExtras::default()
    }
}

/// Haider Code's account-wide weekly allowance window, when published.
#[must_use]
pub fn haider_code_weekly_window(account: &AccountUsageReportV1) -> Option<&UsageWindowV1> {
    if !is_haider_code(&account.provider) {
        return None;
    }
    let AccountMeterStateV1::Metered { windows } = &account.meter else {
        return None;
    };
    windows.iter().find(|window| {
        window.label.is_none() && matches!(window.window.as_str(), "weekly" | "seven_day")
    })
}

/// The two account-wide reset windows the calendar plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    FiveHour,
    Weekly,
}

impl ResetKind {
    pub const BOTH: [Self; 2] = [Self::FiveHour, Self::Weekly];

    /// The grid marker suffix (`a5`, `aW`).
    #[must_use]
    pub const fn marker(self) -> char {
        match self {
            Self::FiveHour => '5',
            Self::Weekly => 'W',
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FiveHour => "5h",
            Self::Weekly => "weekly",
        }
    }
}

/// Pick only provider-published windows whose names have a defined meaning.
/// This never derives a reset by adding five hours or seven days: a missing
/// canonical window/reset stays `reset unknown`. Haider Code publishes one
/// account-wide weekly allowance and no five-hour window.
#[must_use]
pub fn reset_window(account: &AccountUsageReportV1, kind: ResetKind) -> Option<&UsageWindowV1> {
    let AccountMeterStateV1::Metered { windows } = &account.meter else {
        return None;
    };
    let provider = account.provider.to_ascii_lowercase();
    let names: &[&str] = match (provider.as_str(), kind) {
        (provider, ResetKind::FiveHour) if provider.starts_with("anthropic") => &["five_hour"],
        (provider, ResetKind::Weekly) if provider.starts_with("anthropic") => &["seven_day"],
        (provider, ResetKind::FiveHour)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["5h", "five_hour"]
        }
        (provider, ResetKind::Weekly)
            if provider.starts_with("openai") || provider.contains("codex") =>
        {
            &["weekly", "seven_day"]
        }
        (provider, ResetKind::Weekly) if is_haider_code(provider) => &["weekly", "seven_day"],
        _ => return None,
    };
    // OpenAI can publish identically sized named per-model windows. Only an
    // unlabeled provider window is account-wide; even a lone labeled window
    // is not a safe account reset and must stay unknown.
    windows
        .iter()
        .find(|window| window.label.is_none() && names.contains(&window.window.as_str()))
}

/// The grid letter for display index `index` (`a`…`z`, then `27`, `28`…).
#[must_use]
pub fn account_marker(index: usize) -> String {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 26)
        .map_or_else(
            || (index + 1).to_string(),
            |index| char::from(b'a' + index).to_string(),
        )
}

/// The published reset instant of `kind`, when there is one.
#[must_use]
pub fn reset_at(account: &AccountUsageReportV1, kind: ResetKind) -> Option<u64> {
    reset_window(account, kind).and_then(|window| window.resets_at_ms)
}

/// Why a reset of `kind` is unknown, in words (`None` when it is known).
#[must_use]
pub fn reset_unknown_reason(account: &AccountUsageReportV1, kind: ResetKind) -> Option<String> {
    match &account.meter {
        AccountMeterStateV1::Unavailable { reason } => Some(format!(
            "meter unavailable · {}",
            fmt_meter_reason_friendly(reason)
        )),
        AccountMeterStateV1::LocalOnly => Some("local only; no provider meter".to_owned()),
        AccountMeterStateV1::Metered { .. } => match reset_window(account, kind) {
            None => Some("exact window not published".to_owned()),
            Some(window) if window.resets_at_ms.is_none() => {
                Some(format!("{} published no reset", window.window))
            }
            Some(_) => None,
        },
    }
}

#[must_use]
pub fn is_haider_code(provider: &str) -> bool {
    provider.eq_ignore_ascii_case(HAIDER_CODE_PROVIDER)
}

/// Tone of one detail row; the renderer maps it to theme slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTone {
    Normal,
    Dim,
    Warn,
    /// An actionable key hint.
    Action,
    /// A visible but disabled affordance.
    Disabled,
}

/// One row of the detail panel: a left label (empty on continuation rows)
/// and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailRow {
    pub label: &'static str,
    pub text: String,
    pub tone: DetailTone,
}

impl DetailRow {
    fn new(label: &'static str, text: impl Into<String>, tone: DetailTone) -> Self {
        Self {
            label,
            text: text.into(),
            tone,
        }
    }
}

/// The text shown for any field the provider did not report.
pub const NOT_REPORTED: &str = "not reported by provider";

/// Previous resets listed at most (newest first).
pub const HISTORY_ROWS: usize = 3;

/// The panel rows for one account. Pure: the snapshot instant, zone and
/// reference year come from the caller.
#[must_use]
pub fn detail_rows(
    account: &AccountUsageReportV1,
    extras: &AccountResetExtras,
    zone: UsageTimeZone,
    snapshot_ms: u64,
    reference_year: i64,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    if is_haider_code(&account.provider) {
        rows.push(match &extras.allowance {
            None => DetailRow::new("weekly allowance", NOT_REPORTED, DetailTone::Dim),
            Some(allowance) => {
                let left = allowance.utilization.map_or_else(
                    || "remaining not reported".to_owned(),
                    crate::format::fmt_remaining,
                );
                let reset = allowance.resets_at_ms.map_or_else(
                    || "reset not reported".to_owned(),
                    |at| {
                        format!(
                            "resets {} · {}",
                            fmt_reset_instant(at, zone, reference_year),
                            fmt_relative(snapshot_ms, at)
                        )
                    },
                );
                DetailRow::new(
                    "weekly allowance",
                    format!("{left} · {reset} · {}", allowance.window),
                    DetailTone::Normal,
                )
            }
        });
    }
    match &extras.history {
        None => rows.push(DetailRow::new(
            "previous resets",
            NOT_REPORTED,
            DetailTone::Dim,
        )),
        Some(history) if history.is_empty() => {
            rows.push(DetailRow::new(
                "previous resets",
                "none reported",
                DetailTone::Dim,
            ));
        }
        Some(history) => {
            let mut newest = history.clone();
            newest.sort_by_key(|reset| std::cmp::Reverse(reset.at_ms));
            for (index, reset) in newest.iter().take(HISTORY_ROWS).enumerate() {
                rows.push(DetailRow::new(
                    if index == 0 { "previous resets" } else { "" },
                    format!(
                        "{} {} · {} · {}",
                        reset.window,
                        fmt_reset_instant(reset.at_ms, zone, reference_year),
                        fmt_relative(snapshot_ms, reset.at_ms),
                        match reset.source {
                            ResetHistorySource::Provider => "provider",
                            ResetHistorySource::DeviceJournal => "this device's meter journal",
                        }
                    ),
                    DetailTone::Normal,
                ));
            }
            if newest.len() > HISTORY_ROWS {
                rows.push(DetailRow::new(
                    "",
                    format!("+{} older", newest.len() - HISTORY_ROWS),
                    DetailTone::Dim,
                ));
            }
        }
    }
    rows.push(match extras.banked {
        None => DetailRow::new("banked resets", NOT_REPORTED, DetailTone::Dim),
        Some(banked) => {
            let count = |value: Option<u32>| {
                value.map_or_else(|| "not reported".to_owned(), |value| value.to_string())
            };
            DetailRow::new(
                "banked resets",
                format!(
                    "banked {} · used {} · remaining {}",
                    count(banked.banked),
                    count(banked.used),
                    count(banked.remaining)
                ),
                DetailTone::Normal,
            )
        }
    });
    rows.push(match &extras.action {
        BankedResetAction::NotReported => DetailRow::new(
            "use banked reset",
            "no operation reported for this account",
            DetailTone::Disabled,
        ),
        BankedResetAction::NotOffered => DetailRow::new(
            "use banked reset",
            "unavailable — provider offers no banked-reset operation",
            DetailTone::Disabled,
        ),
        BankedResetAction::Unavailable { reason } => DetailRow::new(
            "use banked reset",
            format!("unavailable — {reason}"),
            DetailTone::Disabled,
        ),
        BankedResetAction::Available => DetailRow::new(
            "use banked reset",
            "b · always asks for your approval before spending one",
            DetailTone::Action,
        ),
    });
    rows
}

/// The flash `b` shows when the action is not available for `alias`.
#[must_use]
pub fn banked_reset_unavailable_note(alias: &str, action: &BankedResetAction) -> Option<String> {
    match action {
        BankedResetAction::Available => None,
        BankedResetAction::NotReported => {
            Some(format!("no banked-reset operation reported for {alias}"))
        }
        BankedResetAction::NotOffered => Some(format!(
            "use banked reset: unavailable for {alias} — provider offers no banked-reset operation"
        )),
        BankedResetAction::Unavailable { reason } => Some(format!(
            "use banked reset: unavailable for {alias} — {reason}"
        )),
    }
}

/// The machine meter reason (kept verbatim-compact via
/// [`crate::format::fmt_meter_reason`]) plus, when the reason itself
/// establishes it, what it means and what to do. Never adds facts the
/// reason does not carry: `credential unavailable` is NOT called expired.
#[must_use]
pub fn fmt_meter_reason_friendly(reason: &str) -> String {
    let base = crate::format::fmt_meter_reason(reason);
    let hint = match reason {
        "credential_expired" | "credential_revoked" => Some("re-login with /login"),
        "credential_unavailable" => Some("could not load the login · try /login"),
        "credential_source_gone" => Some("re-link or remove in /accounts"),
        "credential_keychain_locked" => Some("unlock the login keychain"),
        "credential_keychain_denied" => Some("re-allow keychain access or re-link"),
        "credential_source_unreadable" => Some("check permissions or re-link"),
        "credential_origin_client_required" => Some("refresh in the origin client"),
        "credential_policy_blocked" => Some("use a supported credential"),
        "transport_timeout" => Some("provider did not answer · f retries"),
        "transport_error" => Some("network error · f retries"),
        // Emitted when the daemon could not construct its meter HTTP client
        // — before any request, so no network cause is established.
        "transport_unavailable" => Some("meter client unavailable on this device"),
        "malformed_response" => Some("meter reply not understood"),
        _ => reason
            .strip_prefix("http_status_")
            .and_then(|status| match status {
                "401" => Some("unauthorized · try /login"),
                "403" => Some("forbidden by provider"),
                "404" => Some("meter endpoint not found"),
                "429" => Some("rate limited · f retries later"),
                status if status.starts_with('5') => Some("provider error · f retries"),
                _ => None,
            }),
    };
    match hint {
        Some(hint) => format!("{base} — {hint}"),
        None => base,
    }
}
