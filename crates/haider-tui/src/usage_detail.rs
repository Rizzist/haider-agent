//! `/usage` calendar — the selected account's DETAIL panel and the
//! owner-facing meter-failure wording (973 usage UI).
//!
//! The panel is shaped for data the provider may or may not report: the
//! Haider Code weekly allowance, previous resets, banked / used / remaining
//! resets, and a "use banked reset" action. Optional fields retain their
//! absence ("not reported by provider") — nothing is inferred, and
//! no production path fabricates a value.
//!
//! Counts and detail records are carried directly from the published wire.
//! Empty and omitted history share the same wire representation, so the UI
//! makes no claim that a provider explicitly reported an empty history.
//! The daemon owns credit selection, account binding and human approval.

use haider_protocol::usage::{
    AccountMeterStateV1, AccountUsageReportV1, UsageResetApplyResultV1, UsageResetApplyStatusV1,
    UsageResetCreditsV1, UsageResetHistoryEntryV1, UsageResetHistorySourceV1, UsageWindowV1,
};

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

pub type ResetHistorySource = UsageResetHistorySourceV1;
pub type PastReset = UsageResetHistoryEntryV1;
pub type BankedResets = UsageResetCreditsV1;

/// Whether "use banked reset" can be offered for this account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BankedResetAction {
    /// No reset-credit inventory was published. Unknown is never inferred: this is
    /// "not reported", NOT "not offered". Disabled.
    #[default]
    NotReported,
    /// The daemon did not publish a supported operation. Disabled.
    NotOffered,
    /// The provider operation exists but cannot be used now (typed reason,
    /// e.g. `no banked resets remaining`): disabled with that reason.
    Unavailable { reason: String },
    /// Usable. Triggering it always goes through an always-ask approval.
    Available,
}

/// Published extras beyond meter windows. Optional inventory and allowance
/// retain absence; history has only the distinctions carried by the wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountResetExtras {
    /// Haider Code accounts only.
    pub allowance: Option<AllowanceView>,
    /// Empty and omitted history both mean no entries were published.
    pub history: Option<Vec<PastReset>>,
    pub banked: Option<BankedResets>,
    pub action: BankedResetAction,
}

/// Project published fields without narrowing counters or inferring counts.
#[must_use]
pub fn account_extras(account: &AccountUsageReportV1) -> AccountResetExtras {
    AccountResetExtras {
        allowance: haider_code_weekly_window(account).map(|window| AllowanceView {
            window: window.window.clone(),
            utilization: Some(window.utilization),
            resets_at_ms: window.resets_at_ms,
        }),
        history: (!account.previous_resets.is_empty()).then(|| account.previous_resets.clone()),
        banked: account.reset_credits.clone(),
        action: match &account.reset_credits {
            None => BankedResetAction::NotReported,
            Some(credits) if !credits.apply_supported => BankedResetAction::NotOffered,
            Some(credits) if credits.available_count == 0 => BankedResetAction::Unavailable {
                reason: "no available reset credits".into(),
            },
            Some(credits) if credits.applicable_available_count == Some(0) => {
                BankedResetAction::Unavailable {
                    reason: "no applicable reset credits".into(),
                }
            }
            Some(_) => BankedResetAction::Available,
        },
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
        AccountMeterStateV1::Unavailable { reason, .. } => Some(format!(
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
    pub(crate) fn new(label: &'static str, text: impl Into<String>, tone: DetailTone) -> Self {
        Self {
            label,
            text: crate::toolview::sanitize(&text.into()),
            tone,
        }
    }
}

/// The text shown for any field the provider did not report.
pub const NOT_REPORTED: &str = "not reported by provider";

/// No entries on the v1 wire does not distinguish omitted from empty.
pub const NO_HISTORY: &str = "no entries reported (empty or omitted)";

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
    let mut history = extras.history.clone().unwrap_or_default();
    history.sort_by_key(|reset| std::cmp::Reverse(reset.reset_at_ms));
    if history.is_empty() {
        rows.push(DetailRow::new(
            "previous resets",
            NO_HISTORY,
            DetailTone::Dim,
        ));
    }
    for (index, reset) in history.iter().enumerate() {
        rows.push(DetailRow::new(
            if index == 0 { "previous resets" } else { "" },
            format!(
                "{} {} · {} · {}",
                reset.window.as_deref().unwrap_or("window not reported"),
                fmt_reset_instant(reset.reset_at_ms, zone, reference_year),
                fmt_relative(snapshot_ms, reset.reset_at_ms),
                match reset.source {
                    ResetHistorySource::Provider => "provider",
                    ResetHistorySource::DeviceJournal => "this device's meter journal",
                    _ => "source unknown",
                }
            ),
            DetailTone::Normal,
        ));
        rows.push(DetailRow::new(
            "reset kind",
            reset.kind.as_deref().unwrap_or(NOT_REPORTED),
            DetailTone::Normal,
        ));
        rows.push(reset.observed_at_ms.map_or_else(
            || DetailRow::new("observed", NOT_REPORTED, DetailTone::Dim),
            |at| instant_row("observed", at, zone, snapshot_ms, reference_year),
        ));
    }
    match &extras.banked {
        None => rows.push(DetailRow::new(
            "banked resets",
            NOT_REPORTED,
            DetailTone::Dim,
        )),
        Some(credits) => {
            rows.push(DetailRow::new(
                "banked resets",
                format!("available {}", credits.available_count),
                DetailTone::Normal,
            ));
            for (label, value) in [
                ("applicable", credits.applicable_available_count),
                ("total earned", credits.total_earned_count),
                ("used", credits.used_count),
                ("remaining", credits.remaining_count),
            ] {
                rows.push(DetailRow::new(
                    label,
                    value.map_or_else(|| "not reported".to_owned(), |value| value.to_string()),
                    DetailTone::Normal,
                ));
            }
            rows.push(DetailRow::new(
                "apply supported",
                credits.apply_supported.to_string(),
                DetailTone::Normal,
            ));
            if credits.credits.is_empty() {
                rows.push(DetailRow::new(
                    "credit details",
                    NO_HISTORY,
                    DetailTone::Dim,
                ));
            }
            for (index, credit) in credits.credits.iter().enumerate() {
                rows.push(DetailRow::new(
                    "credit details",
                    format!(
                        "{} · type {} · status {}",
                        index + 1,
                        credit.reset_type,
                        credit.status
                    ),
                    DetailTone::Normal,
                ));
                for (label, value) in [
                    ("title", &credit.title),
                    ("description", &credit.description),
                ] {
                    rows.push(DetailRow::new(
                        label,
                        value.as_deref().unwrap_or(NOT_REPORTED),
                        DetailTone::Normal,
                    ));
                }
                for (label, at) in [
                    ("granted", credit.granted_at_ms),
                    ("expires", credit.expires_at_ms),
                    ("redeemed", credit.redeemed_at_ms),
                ] {
                    rows.push(at.map_or_else(
                        || DetailRow::new(label, NOT_REPORTED, DetailTone::Dim),
                        |at| instant_row(label, at, zone, snapshot_ms, reference_year),
                    ));
                }
            }
        }
    }
    if let AccountMeterStateV1::Unavailable { retry_at_ms, .. } = account.meter {
        rows.push(retry_at_ms.map_or_else(
            || DetailRow::new("retry at", NOT_REPORTED, DetailTone::Dim),
            |at| instant_row("retry at", at, zone, snapshot_ms, reference_year),
        ));
    }
    rows.push(match &extras.action {
        BankedResetAction::NotReported => DetailRow::new(
            "use banked reset",
            "no operation reported for this account",
            DetailTone::Disabled,
        ),
        BankedResetAction::NotOffered => DetailRow::new(
            "use banked reset",
            "unavailable — no supported reset operation reported",
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

fn instant_row(
    label: &'static str,
    at: u64,
    zone: UsageTimeZone,
    snapshot_ms: u64,
    year: i64,
) -> DetailRow {
    DetailRow::new(
        label,
        format!(
            "{} · {}",
            fmt_reset_instant(at, zone, year),
            fmt_relative(snapshot_ms, at)
        ),
        DetailTone::Normal,
    )
}

/// Preserve reported status and typed provider reasons without guessing.
#[must_use]
pub fn reset_result_rows(result: &UsageResetApplyResultV1) -> Vec<DetailRow> {
    let status = match result.status {
        UsageResetApplyStatusV1::ApprovalPending => "approval pending",
        UsageResetApplyStatusV1::Applied => "applied",
        UsageResetApplyStatusV1::Denied => "denied",
        UsageResetApplyStatusV1::Failed => "failed",
        _ => "unknown",
    };
    let mut rows = vec![DetailRow::new(
        "reset request",
        format!("{status} · {}", result.approval_id),
        DetailTone::Normal,
    )];
    rows.push(DetailRow::new(
        "windows reset",
        result
            .windows_reset
            .map_or_else(|| NOT_REPORTED.to_owned(), |count| count.to_string()),
        DetailTone::Normal,
    ));
    if let Some(reason) = &result.reason {
        rows.push(DetailRow::new("reset reason", reason, DetailTone::Warn));
    }
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
            "use banked reset: unavailable for {alias} — no supported reset operation reported"
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
